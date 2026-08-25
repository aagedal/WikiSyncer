//! Local IPC and single-writer ownership for a WikiSyncer library.
//!
//! The daemon and short-lived direct writers cooperate through [`WriterLease`].
//! GUI and CLI callers should normally use [`WriterAccess::discover`] instead of
//! opening a writer directly: it forwards to a healthy daemon, acquires the lease
//! when no daemon is present, and reports a busy library in every other case.

mod application;
mod collection;
mod dump_bootstrap;
mod network;
mod purge;
mod schedule;
mod source;

pub use application::ApplicationHandler;
pub use collection::{
    CollectionAdministration, CollectionAdministrationOutcome, CollectionDraft,
    CollectionDraftEstimate, administer_collection_direct, decode_collection_draft,
    encode_collection_draft,
};
pub use dump_bootstrap::{
    CurrentDumpBootstrapOutcome, CurrentDumpBootstrapPreview, CurrentDumpBootstrapRequest,
    bootstrap_collection_from_current_dump_direct,
    bootstrap_collection_from_current_dump_direct_async, preview_current_dump_bootstrap,
};
pub use network::{
    MeteredNetworkProbeOutcome, MeteredNetworkState, MeteredNetworkStatus, detect_metered_network,
};
pub use purge::{
    COLLECTION_PURGE_EXTENSION, COLLECTION_PURGE_RESULT, CollectionPurgeOutcome,
    CollectionPurgeRequest, collection_purge_mutation, decode_collection_purge_outcome,
};
pub use schedule::{
    RecoveryDecision, jittered_occurrence, next_nominal_after, next_occurrence_after, recover,
};
pub use source::{
    MAX_SOURCE_API_ENDPOINT_BYTES, MAX_SOURCE_LANGUAGE_CODE_BYTES, SourceAdministration,
    SourceAdministrationOutcome, administer_source_direct,
};

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Current on-wire request and response contract version.
pub const PROTOCOL_VERSION: u16 = 2;
/// Oldest on-wire contract still accepted by the daemon.
pub const MIN_PROTOCOL_VERSION: u16 = 1;
/// Largest accepted request or response frame, excluding its four-byte length.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Largest opaque mutation payload accepted by the version-one contract.
pub const MAX_MUTATION_PAYLOAD_BYTES: usize = 60 * 1024;
/// Largest chunk used to stage one collection-administration draft.
pub const MAX_COLLECTION_DRAFT_CHUNK_BYTES: usize = 4 * 1024;
/// Largest complete staged collection draft after joining bounded chunks.
pub const MAX_COLLECTION_DRAFT_BYTES: usize = 16 * 1024 * 1024;
/// Library-local daemon request socket name.
pub const DAEMON_SOCKET_NAME: &str = ".wikisyncd.sock";
/// Library-local cooperative writer lease socket name.
pub const WRITER_SOCKET_NAME: &str = ".wikisync-writer.sock";
const IPC_LOCK_NAME: &str = ".wikisync-ipc.lock";
/// Versioned extension name used to configure one collection schedule.
pub const SET_COLLECTION_SCHEDULE_EXTENSION: &str = "set-collection-schedule-v1";
/// Versioned extension name used to configure the library-wide network policy.
pub const SET_NETWORK_TRANSFER_POLICY_EXTENSION: &str = "set-network-transfer-policy-v1";
/// Versioned extension used to execute one authenticated current-dump bootstrap.
pub const SET_CURRENT_DUMP_BOOTSTRAP_EXTENSION: &str = "current-dump-bootstrap-v1";

const REQUEST_MAGIC: &[u8; 4] = b"WKSR";
const RESPONSE_MAGIC: &[u8; 4] = b"WKSP";
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(20);
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);
const LONG_OPERATION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STRING_BYTES: usize = 4 * 1024;

/// Returns the daemon IPC socket path for a library root.
#[must_use]
pub fn daemon_socket_path(library_root: impl AsRef<Path>) -> PathBuf {
    library_root.as_ref().join(DAEMON_SOCKET_NAME)
}

/// Returns the cooperative writer lease socket path for a library root.
#[must_use]
pub fn writer_socket_path(library_root: impl AsRef<Path>) -> PathBuf {
    library_root.as_ref().join(WRITER_SOCKET_NAME)
}

/// Read-only state of one library-local control socket path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalSocketState {
    /// No filesystem entry exists at the expected path.
    Missing,
    /// A Unix socket accepts local connections at the expected path.
    Active,
    /// A Unix socket entry remains but no listener responds.
    Stale,
    /// A symlink or non-socket entry occupies the expected path.
    UnexpectedPath,
}

/// Read-only state of the daemon request socket and cooperative writer lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlPlaneState {
    /// Daemon request socket state.
    pub daemon: LocalSocketState,
    /// Cooperative writer lease socket state.
    pub writer: LocalSocketState,
}

/// Inspects both library-local control sockets without changing the library.
///
/// An existing IPC startup lock is honored. If the lock has not been created yet,
/// inspection takes a bounded read-only snapshot that tolerates concurrent changes.
/// This never creates, removes, or replaces a filesystem entry. The result
/// deliberately contains no paths so it can be included in redacted diagnostics.
pub fn inspect_control_plane(
    library_root: impl AsRef<Path>,
) -> Result<ControlPlaneState, DaemonError> {
    let root = canonical_library_root(library_root.as_ref())?;
    let _lock = IpcLock::acquire_existing(&root)?;
    Ok(ControlPlaneState {
        daemon: inspect_socket(&daemon_socket_path(&root))?.public_state(),
        writer: inspect_socket(&writer_socket_path(&root))?.public_state(),
    })
}

/// A versioned request envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// Protocol contract used to encode this request.
    pub protocol_version: u16,
    /// Caller-selected identifier copied into the response.
    pub request_id: u64,
    /// Requested read or mutation operation.
    pub kind: RequestKind,
}

/// Operations accepted by the daemon foundation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestKind {
    /// Cheap liveness and contract-version check; never mutates the library.
    Health,
    /// Current daemon and handler status; never mutates the library.
    Status,
    /// Forward one serialized writer operation to the daemon.
    Mutate(Mutation),
    /// Stop accepting work after the current request completes.
    Shutdown,
    /// Stage or apply a bounded collection-administration operation.
    ///
    /// This request kind was added in protocol version 2. Callers should normally
    /// use [`Client::administer_collection`] instead of managing staging tokens.
    CollectionAdmin(CollectionAdminRequest),
    /// Applies a bounded source-registration or safe source-removal operation.
    ///
    /// This request kind was added in protocol version 2.
    SourceAdmin(SourceAdministration),
}

/// Low-level protocol operations for one bounded collection-administration draft.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionAdminRequest {
    /// Starts an in-memory draft upload and returns an opaque token.
    Begin { total_bytes: u32 },
    /// Appends the next ordered chunk to the active draft.
    Append {
        /// Opaque token returned by [`Self::Begin`].
        token: u64,
        /// Required byte offset; duplicate and out-of-order chunks are rejected.
        offset: u32,
        /// Bounded encoded draft bytes.
        bytes: Vec<u8>,
    },
    /// Validates and estimates the complete staged draft without consuming it.
    Estimate { token: u64 },
    /// Atomically creates a collection from the complete staged draft.
    Add { token: u64 },
    /// Atomically replaces an active collection from the complete staged draft.
    Edit {
        token: u64,
        collection_id: u64,
        expected_generation: u64,
    },
    /// Tombstones one collection while preserving historical evidence.
    Remove { collection_id: u64 },
    /// Drops the active non-durable staging draft.
    Abort { token: u64 },
}

/// Stable shapes for operations that must run under exclusive writer ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mutation {
    /// Synchronize every configured collection.
    SyncAll,
    /// Synchronize one durable collection identity.
    SyncCollection(u64),
    /// Verify logical objects, optionally requesting complete coverage.
    Verify { full: bool },
    /// Compact immutable object storage without changing logical identities.
    Compact,
    /// A bounded extension point for operations added by application-service code.
    Extension { name: String, payload: Vec<u8> },
}

/// A versioned response envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// Protocol contract used to encode this response.
    pub protocol_version: u16,
    /// Identifier from the corresponding request, or zero if it could not be read.
    pub request_id: u64,
    /// Successful result or structured failure.
    pub kind: ResponseKind,
}

/// Results returned by the daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponseKind {
    /// Liveness information for a compatible daemon.
    Health(Health),
    /// Read-only daemon and application-service status.
    Status(DaemonStatus),
    /// Completed mutation result. Receipt means the handler actually succeeded.
    Mutated(MutationOutcome),
    /// Graceful shutdown was accepted.
    ShutdownAccepted,
    /// Result of one protocol-v2 collection administration step.
    CollectionAdmin(CollectionAdminProtocolOutcome),
    /// Result of one protocol-v2 source-administration operation.
    SourceAdmin(SourceAdministrationOutcome),
    /// Structured rejection. Unsupported operations use [`ErrorCode::Unsupported`].
    Error(ResponseError),
}

/// Low-level results for protocol-v2 collection draft staging and administration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionAdminProtocolOutcome {
    /// A new staging draft was allocated.
    Begun { token: u64 },
    /// One ordered chunk was accepted.
    Appended {
        token: u64,
        received_bytes: u32,
        total_bytes: u32,
    },
    /// The active draft was discarded.
    Aborted { token: u64 },
    /// A complete high-level administration operation finished.
    Completed(CollectionAdministrationOutcome),
}

/// Read-only liveness data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Health {
    /// Daemon package version.
    pub daemon_version: String,
    /// Operating-system process identity.
    pub process_id: u32,
    /// Seconds elapsed since this daemon acquired writer ownership.
    pub uptime_seconds: u64,
}

/// Read-only daemon status plus a handler-supplied application state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonStatus {
    /// Operating-system process identity.
    pub process_id: u32,
    /// Seconds elapsed since daemon startup.
    pub uptime_seconds: u64,
    /// Number of mutations completed successfully by this daemon process.
    pub completed_mutations: u64,
    /// Handler-defined stable state name, such as `idle` or `syncing`.
    pub state: String,
    /// Bounded human-readable detail that must not be interpreted as state.
    pub detail: String,
}

/// Handler-owned portion of a status response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandlerStatus {
    /// Stable state name.
    pub state: String,
    /// Human-readable detail.
    pub detail: String,
}

/// Successful result from a forwarded mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationOutcome {
    /// Stable result name for automation.
    pub result: String,
    /// Bounded handler-specific result bytes.
    pub payload: Vec<u8>,
}

/// Error returned inside a compatible response envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseError {
    /// Machine-readable class.
    pub code: ErrorCode,
    /// Bounded diagnostic text.
    pub message: String,
}

/// Stable version-one response error classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// Frame or request bytes did not satisfy the contract.
    InvalidRequest,
    /// The caller and daemon do not share a protocol version.
    UnsupportedVersion,
    /// The daemon foundation has no handler for this operation.
    Unsupported,
    /// The application-service handler rejected or failed the operation.
    OperationFailed,
    /// An internal daemon error prevented dispatch.
    Internal,
}

/// Application-service dispatch run by the daemon's single writer thread.
pub trait RequestHandler: fmt::Debug + Send + 'static {
    /// Recovers durable work after writer ownership is acquired and before the
    /// daemon socket accepts requests.
    fn startup(&mut self, _control: OperationControl) -> Result<(), OperationError> {
        Ok(())
    }

    /// Produces read-only application status.
    fn status(&self) -> HandlerStatus;

    /// Performs one mutation synchronously under daemon writer ownership.
    /// Returning success must mean the requested work actually completed.
    fn mutate(
        &mut self,
        mutation: Mutation,
        control: OperationControl,
    ) -> Result<MutationOutcome, OperationError>;

    /// Stages or applies one protocol-v2 collection administration operation.
    fn administer_collection(
        &mut self,
        _request: CollectionAdminRequest,
        _control: OperationControl,
    ) -> Result<CollectionAdminProtocolOutcome, OperationError> {
        Err(OperationError::unsupported(
            "this handler does not implement collection administration",
        ))
    }

    /// Applies one protocol-v2 source-administration operation.
    fn administer_source(
        &mut self,
        _administration: SourceAdministration,
        _control: OperationControl,
    ) -> Result<SourceAdministrationOutcome, OperationError> {
        Err(OperationError::unsupported(
            "this handler does not implement source administration",
        ))
    }

    /// Polls for at most one durably claimed background operation between requests.
    fn poll_background(
        &mut self,
        _control: OperationControl,
    ) -> Result<Option<MutationOutcome>, OperationError> {
        Ok(None)
    }
}

/// Cooperative cancellation state for one active foreground or background operation.
///
/// The control shares the daemon's shutdown flag. Long-running handlers should clone
/// it as needed and check [`Self::is_shutdown_requested`] at bounded intervals before
/// claiming more work and between durable checkpoints.
#[derive(Clone, Debug)]
pub struct OperationControl {
    running: Arc<AtomicBool>,
}

impl OperationControl {
    /// Creates an independent control for a short-lived direct operation.
    ///
    /// This control begins in the running state and is not connected to daemon
    /// process signals or any [`ShutdownHandle`].
    #[must_use]
    pub fn running() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Returns whether shutdown was requested through this control's associated owner.
    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        !self.running.load(Ordering::Acquire)
    }
}

/// Creates the bounded version-one extension mutation for a collection schedule.
#[must_use]
pub fn set_collection_schedule_mutation(
    collection_id: u64,
    cadence: wikisync_store::ScheduleCadence,
    jitter_seconds: u32,
    paused: bool,
) -> Mutation {
    let (kind, value) = match cadence {
        wikisync_store::ScheduleCadence::Manual => (0_u8, 0_u32),
        wikisync_store::ScheduleCadence::Interval(interval) => (1, interval.seconds()),
        wikisync_store::ScheduleCadence::DailyUtc(time) => (2, time.seconds_after_midnight()),
    };
    let mut payload = Vec::with_capacity(18);
    payload.extend_from_slice(&collection_id.to_be_bytes());
    payload.push(kind);
    payload.extend_from_slice(&value.to_be_bytes());
    payload.extend_from_slice(&jitter_seconds.to_be_bytes());
    payload.push(u8::from(paused));
    Mutation::Extension {
        name: SET_COLLECTION_SCHEDULE_EXTENSION.to_owned(),
        payload,
    }
}

/// Creates the bounded version-one extension mutation for the network transfer policy.
#[must_use]
pub fn set_network_transfer_policy_mutation(
    policy: wikisync_store::NetworkTransferPolicy,
) -> Mutation {
    let mut payload = Vec::with_capacity(13);
    payload.extend_from_slice(&policy.max_concurrent_requests().to_be_bytes());
    payload.extend_from_slice(
        &policy
            .max_download_bytes_per_second()
            .unwrap_or(0)
            .to_be_bytes(),
    );
    payload.push(u8::from(policy.avoid_metered_networks()));
    Mutation::Extension {
        name: SET_NETWORK_TRANSFER_POLICY_EXTENSION.to_owned(),
        payload,
    }
}

/// Structured application-service failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationError {
    code: ErrorCode,
    message: String,
}

impl OperationError {
    /// Reports that no application service implements this mutation yet.
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Unsupported,
            message: message.into(),
        }
    }

    /// Reports a failed mutation that may have durable resumable progress.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::OperationFailed,
            message: message.into(),
        }
    }

    /// Returns the machine-readable operation failure class.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// Returns the human-readable operation failure detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for OperationError {}

/// A no-op handler used by the initial binary until sync dispatch is integrated.
#[derive(Debug, Default)]
pub struct FoundationHandler;

impl RequestHandler for FoundationHandler {
    fn status(&self) -> HandlerStatus {
        HandlerStatus {
            state: "idle".to_owned(),
            detail: "single-writer IPC foundation is ready; scheduling is not configured"
                .to_owned(),
        }
    }

    fn mutate(
        &mut self,
        _mutation: Mutation,
        _control: OperationControl,
    ) -> Result<MutationOutcome, OperationError> {
        Err(OperationError::unsupported(
            "this daemon build has no application-service mutation dispatcher",
        ))
    }
}

/// Exclusive cooperative ownership required before opening a library writer.
///
/// The lease is a private Unix socket rather than a stale PID file. Socket binding is
/// atomic, and a small monitor drains probes while the owner is alive. Stale socket
/// names fail closed instead of risking removal of a concurrently replaced socket.
#[derive(Debug)]
pub struct WriterLease {
    stop: Arc<AtomicBool>,
    monitor: Option<JoinHandle<()>>,
    socket: SocketGuard,
}

impl WriterLease {
    /// Acquires exclusive writer ownership for this library.
    pub fn acquire(library_root: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let root = canonical_library_root(library_root.as_ref())?;
        let path = writer_socket_path(&root);
        let (listener, socket) = bind_exclusive(&root, &path, BusyKind::Writer)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let monitor_stop = Arc::clone(&stop);
        let monitor = thread::Builder::new()
            .name("wikisync-writer-lease".to_owned())
            .spawn(move || {
                while !monitor_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((_stream, _address)) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) =>
                        {
                            thread::sleep(SOCKET_POLL_INTERVAL);
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            stop,
            monitor: Some(monitor),
            socket,
        })
    }

    /// Path whose ownership represents this guard.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.socket.path
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

/// Race-safe result for a caller that needs to mutate a library.
#[derive(Debug)]
pub enum WriterAccess {
    /// A compatible daemon owns the library; forward the mutation through this client.
    Daemon(Client),
    /// No daemon owns the library; the caller may write while holding this lease.
    Direct(WriterLease),
}

impl WriterAccess {
    /// Detects a healthy daemon or acquires direct writer ownership when absent.
    pub fn discover(library_root: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let client = Client::for_library(library_root.as_ref())?;
        if client.is_running()? {
            return Ok(Self::Daemon(client));
        }
        match WriterLease::acquire(library_root.as_ref()) {
            Ok(lease) => Ok(Self::Direct(lease)),
            Err(DaemonError::WriterBusy) => {
                if client.is_running()? {
                    Ok(Self::Daemon(client))
                } else {
                    Err(DaemonError::WriterBusy)
                }
            }
            Err(error) => Err(error),
        }
    }
}

/// A client for one library-local daemon.
#[derive(Clone, Debug)]
pub struct Client {
    socket_path: PathBuf,
    next_request_id: Arc<AtomicU64>,
}

impl Client {
    /// Creates a client without opening a persistent connection.
    pub fn for_library(library_root: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let root = canonical_library_root(library_root.as_ref())?;
        Ok(Self {
            socket_path: daemon_socket_path(root),
            next_request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Returns true only when a compatible daemon answers a health request.
    pub fn is_running(&self) -> Result<bool, DaemonError> {
        match self.health() {
            Ok(_) => Ok(true),
            Err(DaemonError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Requests read-only liveness information.
    pub fn health(&self) -> Result<Health, DaemonError> {
        match self.call_compatible(RequestKind::Health)? {
            ResponseKind::Health(health) => Ok(health),
            kind => Err(unexpected_response("health", kind)),
        }
    }

    /// Requests read-only daemon and application status.
    pub fn status(&self) -> Result<DaemonStatus, DaemonError> {
        match self.call_compatible(RequestKind::Status)? {
            ResponseKind::Status(status) => Ok(status),
            kind => Err(unexpected_response("status", kind)),
        }
    }

    /// Forwards a mutation to the process holding daemon writer ownership.
    pub fn forward_mutation(&self, mutation: Mutation) -> Result<MutationOutcome, DaemonError> {
        match self.call_compatible(RequestKind::Mutate(mutation))? {
            ResponseKind::Mutated(outcome) => Ok(outcome),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("mutation", kind)),
        }
    }

    /// Executes one authenticated current-dump bootstrap through the daemon writer.
    pub fn bootstrap_collection_from_current_dump(
        &self,
        request: &CurrentDumpBootstrapRequest,
    ) -> Result<CurrentDumpBootstrapOutcome, DaemonError> {
        let outcome =
            self.forward_mutation(dump_bootstrap::current_dump_bootstrap_mutation(request)?)?;
        if outcome.result != "current-dump-bootstrap-complete" {
            return Err(DaemonError::Protocol(
                "unexpected current-dump bootstrap result name",
            ));
        }
        dump_bootstrap::decode_current_dump_bootstrap_outcome(&outcome.payload)
    }

    /// Requests graceful shutdown after the daemon's current request completes.
    pub fn shutdown(&self) -> Result<(), DaemonError> {
        match self.call_compatible(RequestKind::Shutdown)? {
            ResponseKind::ShutdownAccepted => Ok(()),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("shutdown", kind)),
        }
    }

    /// Runs one bounded collection operation through the daemon writer.
    ///
    /// Complete previews are encoded once and uploaded in ordered chunks so even a
    /// 10,000-page preview never weakens the 64 KiB frame ceiling. Failed uploads are
    /// aborted on a best-effort basis; daemon-side expiry is the final cleanup bound.
    pub fn administer_collection(
        &self,
        administration: CollectionAdministration,
    ) -> Result<CollectionAdministrationOutcome, DaemonError> {
        match administration {
            CollectionAdministration::Remove { collection_id } => self
                .collection_admin_call(CollectionAdminRequest::Remove {
                    collection_id: collection_id.get(),
                })
                .and_then(completed_collection_outcome),
            CollectionAdministration::Estimate(draft) => {
                self.stage_collection_draft(draft, None, |token| {
                    self.collection_admin_call(CollectionAdminRequest::Estimate { token })
                        .and_then(completed_collection_outcome)
                })
            }
            CollectionAdministration::Add(draft) => {
                self.stage_collection_draft(draft, None, |token| {
                    self.collection_admin_call(CollectionAdminRequest::Add { token })
                        .and_then(completed_collection_outcome)
                })
            }
            CollectionAdministration::AddWithImagePolicy {
                draft,
                image_policy,
            } => self.stage_collection_draft(draft, Some(image_policy), |token| {
                self.collection_admin_call(CollectionAdminRequest::Add { token })
                    .and_then(completed_collection_outcome)
            }),
            CollectionAdministration::Edit {
                collection_id,
                expected_generation,
                draft,
            } => self.stage_collection_draft(draft, None, |token| {
                self.collection_admin_call(CollectionAdminRequest::Edit {
                    token,
                    collection_id: collection_id.get(),
                    expected_generation,
                })
                .and_then(completed_collection_outcome)
            }),
            CollectionAdministration::EditWithImagePolicy {
                collection_id,
                expected_generation,
                draft,
                image_policy,
            } => self.stage_collection_draft(draft, Some(image_policy), |token| {
                self.collection_admin_call(CollectionAdminRequest::Edit {
                    token,
                    collection_id: collection_id.get(),
                    expected_generation,
                })
                .and_then(completed_collection_outcome)
            }),
        }
    }

    /// Runs one validated source operation through the daemon writer.
    pub fn administer_source(
        &self,
        administration: SourceAdministration,
    ) -> Result<SourceAdministrationOutcome, DaemonError> {
        match self.call(RequestKind::SourceAdmin(administration))? {
            ResponseKind::SourceAdmin(outcome) => Ok(outcome),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("source-admin", kind)),
        }
    }

    fn stage_collection_draft(
        &self,
        draft: CollectionDraft,
        image_policy: Option<wikisync_core::ImagePolicy>,
        finish: impl FnOnce(u64) -> Result<CollectionAdministrationOutcome, DaemonError>,
    ) -> Result<CollectionAdministrationOutcome, DaemonError> {
        let encoded = match image_policy {
            Some(image_policy) => {
                collection::encode_collection_draft_with_image_policy(&draft, image_policy)
            }
            None => encode_collection_draft(&draft),
        }
        .map_err(|_| DaemonError::Protocol("collection draft failed local validation"))?;
        let total_bytes = u32::try_from(encoded.len())
            .map_err(|_| DaemonError::Protocol("collection draft is too large"))?;
        let token =
            match self.collection_admin_call(CollectionAdminRequest::Begin { total_bytes })? {
                CollectionAdminProtocolOutcome::Begun { token } => token,
                _ => {
                    return Err(DaemonError::Protocol(
                        "unexpected collection begin response",
                    ));
                }
            };
        let staged = (|| {
            for (index, chunk) in encoded.chunks(MAX_COLLECTION_DRAFT_CHUNK_BYTES).enumerate() {
                let offset = index
                    .checked_mul(MAX_COLLECTION_DRAFT_CHUNK_BYTES)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(DaemonError::Protocol("collection draft offset overflowed"))?;
                match self.collection_admin_call(CollectionAdminRequest::Append {
                    token,
                    offset,
                    bytes: chunk.to_vec(),
                })? {
                    CollectionAdminProtocolOutcome::Appended {
                        token: response_token,
                        received_bytes,
                        total_bytes: response_total,
                    } if response_token == token
                        && response_total == total_bytes
                        && received_bytes
                            == offset + u32::try_from(chunk.len()).unwrap_or(u32::MAX) => {}
                    _ => {
                        return Err(DaemonError::Protocol(
                            "unexpected collection append response",
                        ));
                    }
                }
            }
            finish(token)
        })();
        if staged.is_err() || matches!(&staged, Ok(CollectionAdministrationOutcome::Estimated(_))) {
            let _ = self.collection_admin_call(CollectionAdminRequest::Abort { token });
        }
        staged
    }

    fn collection_admin_call(
        &self,
        request: CollectionAdminRequest,
    ) -> Result<CollectionAdminProtocolOutcome, DaemonError> {
        match self.call(RequestKind::CollectionAdmin(request))? {
            ResponseKind::CollectionAdmin(outcome) => Ok(outcome),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("collection-admin", kind)),
        }
    }

    /// Sends any public request kind using the current protocol version.
    pub fn call(&self, kind: RequestKind) -> Result<ResponseKind, DaemonError> {
        self.call_version(kind, PROTOCOL_VERSION)
    }

    fn call_compatible(&self, kind: RequestKind) -> Result<ResponseKind, DaemonError> {
        match self.call_version(kind.clone(), PROTOCOL_VERSION) {
            Err(DaemonError::Remote(ResponseError {
                code: ErrorCode::UnsupportedVersion,
                ..
            })) => self.call_version(kind, MIN_PROTOCOL_VERSION),
            result => result,
        }
    }

    fn call_version(
        &self,
        kind: RequestKind,
        protocol_version: u16,
    ) -> Result<ResponseKind, DaemonError> {
        let read_timeout = if matches!(
            kind,
            RequestKind::Mutate(_)
                | RequestKind::Shutdown
                | RequestKind::CollectionAdmin(_)
                | RequestKind::SourceAdmin(_)
        ) {
            LONG_OPERATION_TIMEOUT
        } else {
            CLIENT_IO_TIMEOUT
        };
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            protocol_version,
            request_id,
            kind,
        };
        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
        write_frame(&mut stream, &encode_request(&request)?)?;
        let response = decode_response(&read_frame(&mut stream)?)?;
        if response.request_id != request_id {
            return Err(DaemonError::Protocol(
                "daemon response ID did not match request",
            ));
        }
        if let ResponseKind::Error(error) = &response.kind {
            if error.code == ErrorCode::UnsupportedVersion {
                return Err(DaemonError::Remote(error.clone()));
            }
        }
        if response.protocol_version != protocol_version {
            return Err(DaemonError::Protocol("daemon response version changed"));
        }
        Ok(response.kind)
    }
}

/// Single-threaded dispatcher that owns the long-lived writer lease.
#[derive(Debug)]
pub struct Daemon<H: RequestHandler> {
    listener: UnixListener,
    _socket: SocketGuard,
    _writer_lease: WriterLease,
    handler: H,
    running: Arc<AtomicBool>,
    started: Instant,
    completed_mutations: u64,
    last_background_poll: Instant,
}

impl<H: RequestHandler> Daemon<H> {
    /// Acquires writer ownership and binds private library-local IPC.
    pub fn bind(library_root: impl AsRef<Path>, mut handler: H) -> Result<Self, DaemonError> {
        let root = canonical_library_root(library_root.as_ref())?;
        let writer_lease = WriterLease::acquire(&root)?;
        let running = Arc::new(AtomicBool::new(true));
        handler
            .startup(OperationControl {
                running: Arc::clone(&running),
            })
            .map_err(|error| DaemonError::Startup(error.to_string()))?;
        let path = daemon_socket_path(&root);
        let (listener, socket) = bind_exclusive(&root, &path, BusyKind::Daemon)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            _socket: socket,
            _writer_lease: writer_lease,
            handler,
            running,
            started: Instant::now(),
            completed_mutations: 0,
            last_background_poll: Instant::now(),
        })
    }

    /// Returns a process-local handle that requests shutdown between IPC requests.
    #[must_use]
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            running: Arc::clone(&self.running),
        }
    }

    fn operation_control(&self) -> OperationControl {
        OperationControl {
            running: Arc::clone(&self.running),
        }
    }

    /// Serves requests until an IPC or process-local graceful shutdown request.
    pub fn run(mut self) -> Result<(), DaemonError> {
        while self.running.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((mut stream, _address)) => {
                    if stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT)).is_err()
                        || stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT)).is_err()
                    {
                        continue;
                    }
                    // A malformed, timed-out, or disconnected local client must not
                    // terminate the long-lived writer. Contract errors are answered
                    // by `serve_one` when the connection is still writable.
                    let _ = self.serve_one(&mut stream);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    if self.last_background_poll.elapsed() >= BACKGROUND_POLL_INTERVAL {
                        self.last_background_poll = Instant::now();
                        let control = self.operation_control();
                        if matches!(self.handler.poll_background(control), Ok(Some(_))) {
                            self.completed_mutations = self.completed_mutations.saturating_add(1);
                        }
                    }
                    thread::sleep(SOCKET_POLL_INTERVAL);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn serve_one(&mut self, stream: &mut UnixStream) -> Result<(), DaemonError> {
        let bytes = match read_frame(stream) {
            Ok(bytes) => bytes,
            Err(DaemonError::FrameTooLarge { .. }) => {
                let response =
                    error_response(0, ErrorCode::InvalidRequest, "request frame is too large");
                write_frame(stream, &encode_response(&response)?)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let request = match decode_request(&bytes) {
            Ok(request) => request,
            Err(DecodeRequestError::UnsupportedVersion {
                request_id,
                version,
            }) => {
                let response = error_response(
                    request_id,
                    ErrorCode::UnsupportedVersion,
                    &format!(
                        "protocol version {version} is unsupported; supported range is {MIN_PROTOCOL_VERSION} through {PROTOCOL_VERSION}"
                    ),
                );
                write_frame(stream, &encode_response(&response)?)?;
                return Ok(());
            }
            Err(DecodeRequestError::Invalid {
                request_id,
                message,
            }) => {
                let response =
                    error_response(request_id.unwrap_or(0), ErrorCode::InvalidRequest, message);
                write_frame(stream, &encode_response(&response)?)?;
                return Ok(());
            }
        };
        let response = self.dispatch(request);
        write_frame(stream, &encode_response(&response)?)?;
        Ok(())
    }

    fn dispatch(&mut self, request: Request) -> Response {
        let kind = match request.kind {
            RequestKind::Health => ResponseKind::Health(Health {
                daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                process_id: std::process::id(),
                uptime_seconds: self.started.elapsed().as_secs(),
            }),
            RequestKind::Status => {
                let handler = self.handler.status();
                if handler.state.len() > MAX_STRING_BYTES || handler.detail.len() > MAX_STRING_BYTES
                {
                    ResponseKind::Error(ResponseError {
                        code: ErrorCode::Internal,
                        message: "handler returned an oversized status field".to_owned(),
                    })
                } else {
                    ResponseKind::Status(DaemonStatus {
                        process_id: std::process::id(),
                        uptime_seconds: self.started.elapsed().as_secs(),
                        completed_mutations: self.completed_mutations,
                        state: handler.state,
                        detail: handler.detail,
                    })
                }
            }
            RequestKind::Mutate(mutation) => {
                let control = self.operation_control();
                dispatch_mutation(
                    &mut self.handler,
                    mutation,
                    control,
                    &mut self.completed_mutations,
                )
            }
            RequestKind::Shutdown => {
                self.running.store(false, Ordering::Release);
                ResponseKind::ShutdownAccepted
            }
            RequestKind::CollectionAdmin(request) => {
                let is_durable_mutation = matches!(
                    request,
                    CollectionAdminRequest::Add { .. }
                        | CollectionAdminRequest::Edit { .. }
                        | CollectionAdminRequest::Remove { .. }
                );
                let control = self.operation_control();
                match self.handler.administer_collection(request, control) {
                    Ok(outcome) => {
                        if is_durable_mutation
                            && matches!(outcome, CollectionAdminProtocolOutcome::Completed(_))
                        {
                            self.completed_mutations = self.completed_mutations.saturating_add(1);
                        }
                        ResponseKind::CollectionAdmin(outcome)
                    }
                    Err(error) => ResponseKind::Error(ResponseError {
                        code: error.code,
                        message: bounded_message(error.message),
                    }),
                }
            }
            RequestKind::SourceAdmin(administration) => {
                let control = self.operation_control();
                match self.handler.administer_source(administration, control) {
                    Ok(outcome) => {
                        self.completed_mutations = self.completed_mutations.saturating_add(1);
                        ResponseKind::SourceAdmin(outcome)
                    }
                    Err(error) => ResponseKind::Error(ResponseError {
                        code: error.code,
                        message: bounded_message(error.message),
                    }),
                }
            }
        };
        Response {
            protocol_version: request.protocol_version,
            request_id: request.request_id,
            kind,
        }
    }
}

fn completed_collection_outcome(
    outcome: CollectionAdminProtocolOutcome,
) -> Result<CollectionAdministrationOutcome, DaemonError> {
    match outcome {
        CollectionAdminProtocolOutcome::Completed(outcome) => Ok(outcome),
        _ => Err(DaemonError::Protocol(
            "unexpected completed collection administration response",
        )),
    }
}

fn dispatch_mutation<H: RequestHandler>(
    handler: &mut H,
    mutation: Mutation,
    control: OperationControl,
    completed_mutations: &mut u64,
) -> ResponseKind {
    match handler.mutate(mutation, control) {
        Ok(outcome)
            if outcome.result.len() <= MAX_STRING_BYTES
                && outcome.payload.len() <= MAX_MUTATION_PAYLOAD_BYTES =>
        {
            *completed_mutations = completed_mutations.saturating_add(1);
            ResponseKind::Mutated(outcome)
        }
        Ok(_) => ResponseKind::Error(ResponseError {
            code: ErrorCode::Internal,
            message: "handler returned an oversized mutation result".to_owned(),
        }),
        Err(error) => ResponseKind::Error(ResponseError {
            code: error.code,
            message: bounded_message(error.message),
        }),
    }
}

/// Process-local graceful shutdown control.
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    running: Arc<AtomicBool>,
}

impl ShutdownHandle {
    /// Stops the accept loop after the active request completes.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Release);
    }
}

/// Failures from ownership, transport, contract validation, or remote dispatch.
#[derive(Debug)]
pub enum DaemonError {
    /// Filesystem or Unix socket failure.
    Io(io::Error),
    /// Another daemon owns this library.
    AlreadyRunning,
    /// A daemon or direct operation already owns the writer lease.
    WriterBusy,
    /// The library root is absent or not a directory.
    InvalidLibrary(PathBuf),
    /// A stale socket changed identity before coordinated recovery could remove it.
    StaleSocket(PathBuf),
    /// A bounded frame exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge { size: usize },
    /// Malformed or inconsistent protocol data.
    Protocol(&'static str),
    /// Application recovery failed while writer ownership was held.
    Startup(String),
    /// Compatible remote daemon rejected the request.
    Remote(ResponseError),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "daemon I/O failed: {error}"),
            Self::AlreadyRunning => formatter.write_str("a daemon already owns this library"),
            Self::WriterBusy => formatter.write_str("another process owns this library writer"),
            Self::InvalidLibrary(path) => write!(
                formatter,
                "{} is not an existing library directory",
                path.display()
            ),
            Self::StaleSocket(path) => write!(
                formatter,
                "stale socket {} changed during recovery; retry without removing it manually",
                path.display()
            ),
            Self::FrameTooLarge { size } => write!(
                formatter,
                "IPC frame is {size} bytes; maximum is {MAX_FRAME_BYTES}"
            ),
            Self::Protocol(message) => write!(formatter, "invalid daemon protocol: {message}"),
            Self::Startup(message) => {
                write!(formatter, "daemon startup recovery failed: {message}")
            }
            Self::Remote(error) => write!(
                formatter,
                "daemon rejected request ({:?}): {}",
                error.code, error.message
            ),
        }
    }
}

impl Error for DaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    fn new(path: PathBuf) -> io::Result<Self> {
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let owned = fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        });
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BusyKind {
    Daemon,
    Writer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketInspection {
    Missing,
    Active,
    Stale(SocketIdentity),
    UnexpectedPath,
}

impl SocketInspection {
    const fn public_state(self) -> LocalSocketState {
        match self {
            Self::Missing => LocalSocketState::Missing,
            Self::Active => LocalSocketState::Active,
            Self::Stale(_) => LocalSocketState::Stale,
            Self::UnexpectedPath => LocalSocketState::UnexpectedPath,
        }
    }
}

#[derive(Debug)]
struct IpcLock {
    _file: File,
}

impl IpcLock {
    fn acquire(library_root: &Path) -> Result<Self, DaemonError> {
        let path = library_root.join(IPC_LOCK_NAME);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                reject_unexpected_lock_path(&path)?;
                OpenOptions::new().read(true).write(true).open(&path)?
            }
            Err(error) => return Err(error.into()),
        };
        validate_open_lock_file(&path, &file)?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        FileExt::lock_exclusive(&file)?;
        validate_open_lock_file(&path, &file)?;
        Ok(Self { _file: file })
    }

    fn acquire_existing(library_root: &Path) -> Result<Option<Self>, DaemonError> {
        let path = library_root.join(IPC_LOCK_NAME);
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        validate_open_lock_file(&path, &file)?;
        FileExt::lock_exclusive(&file)?;
        validate_open_lock_file(&path, &file)?;
        Ok(Some(Self { _file: file }))
    }
}

fn reject_unexpected_lock_path(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(unexpected_path_error(path, "IPC lock"));
    }
    Ok(())
}

fn validate_open_lock_file(path: &Path, file: &File) -> Result<(), DaemonError> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if !path_metadata.file_type().is_file()
        || path_metadata.dev() != file_metadata.dev()
        || path_metadata.ino() != file_metadata.ino()
    {
        return Err(unexpected_path_error(path, "IPC lock"));
    }
    Ok(())
}

fn bind_exclusive(
    library_root: &Path,
    path: &Path,
    busy: BusyKind,
) -> Result<(UnixListener, SocketGuard), DaemonError> {
    let _lock = IpcLock::acquire(library_root)?;
    match inspect_socket(path)? {
        SocketInspection::Missing => bind_new_socket(path),
        SocketInspection::Active => Err(busy_error(busy)),
        SocketInspection::UnexpectedPath => Err(unexpected_path_error(path, "control socket")),
        SocketInspection::Stale(identity) => {
            remove_confirmed_stale_socket(path, identity)?;
            bind_new_socket(path)
        }
    }
}

fn bind_new_socket(path: &Path) -> Result<(UnixListener, SocketGuard), DaemonError> {
    let listener = UnixListener::bind(path)?;
    let guard = SocketGuard::new(path.to_path_buf())?;
    Ok((listener, guard))
}

fn busy_error(busy: BusyKind) -> DaemonError {
    match busy {
        BusyKind::Daemon => DaemonError::AlreadyRunning,
        BusyKind::Writer => DaemonError::WriterBusy,
    }
}

fn inspect_socket(path: &Path) -> Result<SocketInspection, DaemonError> {
    for _attempt in 0..3 {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SocketInspection::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_socket() {
            return Ok(SocketInspection::UnexpectedPath);
        }
        let identity = SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        match UnixStream::connect(path) {
            Ok(_) => return Ok(SocketInspection::Active),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                match socket_identity(path)? {
                    None => return Ok(SocketInspection::Missing),
                    Some(current) if current == identity => {
                        return Ok(SocketInspection::Stale(identity));
                    }
                    Some(_) => continue,
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(DaemonError::Io(io::Error::new(
        io::ErrorKind::WouldBlock,
        "control socket changed repeatedly during inspection",
    )))
}

fn socket_identity(path: &Path) -> Result<Option<SocketIdentity>, DaemonError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(Some(SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_confirmed_stale_socket(path: &Path, expected: SocketIdentity) -> Result<(), DaemonError> {
    if socket_identity(path)? != Some(expected) {
        return Err(DaemonError::StaleSocket(path.to_path_buf()));
    }
    fs::remove_file(path)?;
    Ok(())
}

fn unexpected_path_error(path: &Path, kind: &str) -> DaemonError {
    DaemonError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "refusing to replace unexpected {kind} path {}",
            path.display()
        ),
    ))
}

fn canonical_library_root(path: &Path) -> Result<PathBuf, DaemonError> {
    if !path.is_dir() {
        return Err(DaemonError::InvalidLibrary(path.to_path_buf()));
    }
    Ok(fs::canonicalize(path)?)
}

fn unexpected_response(operation: &'static str, kind: ResponseKind) -> DaemonError {
    match kind {
        ResponseKind::Error(error) => DaemonError::Remote(error),
        _ => DaemonError::Protocol(match operation {
            "health" => "unexpected response to health request",
            "status" => "unexpected response to status request",
            "mutation" => "unexpected response to mutation request",
            "shutdown" => "unexpected response to shutdown request",
            _ => "unexpected response kind",
        }),
    }
}

fn error_response(request_id: u64, code: ErrorCode, message: &str) -> Response {
    Response {
        protocol_version: PROTOCOL_VERSION,
        request_id,
        kind: ResponseKind::Error(ResponseError {
            code,
            message: bounded_message(message.to_owned()),
        }),
    }
}

fn bounded_message(mut message: String) -> String {
    if message.len() > MAX_STRING_BYTES {
        let mut end = MAX_STRING_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> Result<(), DaemonError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(DaemonError::FrameTooLarge { size: bytes.len() });
    }
    let length =
        u32::try_from(bytes.len()).map_err(|_| DaemonError::FrameTooLarge { size: bytes.len() })?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, DaemonError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(DaemonError::FrameTooLarge { size: length });
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn encode_request(request: &Request) -> Result<Vec<u8>, DaemonError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(REQUEST_MAGIC);
    put_u16(&mut bytes, request.protocol_version);
    put_u64(&mut bytes, request.request_id);
    match &request.kind {
        RequestKind::Health => put_u8(&mut bytes, 1),
        RequestKind::Status => put_u8(&mut bytes, 2),
        RequestKind::Mutate(mutation) => {
            put_u8(&mut bytes, 3);
            encode_mutation(&mut bytes, mutation)?;
        }
        RequestKind::Shutdown => put_u8(&mut bytes, 4),
        RequestKind::CollectionAdmin(admin_request) => {
            if request.protocol_version < 2 {
                return Err(DaemonError::Protocol(
                    "collection administration requires protocol version 2",
                ));
            }
            put_u8(&mut bytes, 5);
            encode_collection_admin_request(&mut bytes, admin_request)?;
        }
        RequestKind::SourceAdmin(administration) => {
            if request.protocol_version < 2 {
                return Err(DaemonError::Protocol(
                    "source administration requires protocol version 2",
                ));
            }
            put_u8(&mut bytes, 6);
            encode_source_administration(&mut bytes, administration)?;
        }
    }
    ensure_frame_size(bytes)
}

fn encode_source_administration(
    bytes: &mut Vec<u8>,
    administration: &SourceAdministration,
) -> Result<(), DaemonError> {
    match administration {
        SourceAdministration::Add {
            api_endpoint,
            language_code,
        } => {
            put_u8(bytes, 1);
            put_bytes(
                bytes,
                api_endpoint.as_bytes(),
                MAX_SOURCE_API_ENDPOINT_BYTES,
            )?;
            put_bytes(
                bytes,
                language_code.as_bytes(),
                MAX_SOURCE_LANGUAGE_CODE_BYTES,
            )?;
        }
        SourceAdministration::Remove { wiki_id } => {
            put_u8(bytes, 2);
            put_u64(bytes, wiki_id.get());
        }
    }
    Ok(())
}

fn encode_collection_admin_request(
    bytes: &mut Vec<u8>,
    request: &CollectionAdminRequest,
) -> Result<(), DaemonError> {
    match request {
        CollectionAdminRequest::Begin { total_bytes } => {
            if *total_bytes == 0 || *total_bytes as usize > MAX_COLLECTION_DRAFT_BYTES {
                return Err(DaemonError::FrameTooLarge {
                    size: *total_bytes as usize,
                });
            }
            put_u8(bytes, 1);
            put_u32(bytes, *total_bytes);
        }
        CollectionAdminRequest::Append {
            token,
            offset,
            bytes: chunk,
        } => {
            put_u8(bytes, 2);
            put_u64(bytes, *token);
            put_u32(bytes, *offset);
            put_bytes(bytes, chunk, MAX_COLLECTION_DRAFT_CHUNK_BYTES)?;
        }
        CollectionAdminRequest::Estimate { token } => {
            put_u8(bytes, 3);
            put_u64(bytes, *token);
        }
        CollectionAdminRequest::Add { token } => {
            put_u8(bytes, 4);
            put_u64(bytes, *token);
        }
        CollectionAdminRequest::Edit {
            token,
            collection_id,
            expected_generation,
        } => {
            put_u8(bytes, 5);
            put_u64(bytes, *token);
            put_u64(bytes, *collection_id);
            put_u64(bytes, *expected_generation);
        }
        CollectionAdminRequest::Remove { collection_id } => {
            put_u8(bytes, 6);
            put_u64(bytes, *collection_id);
        }
        CollectionAdminRequest::Abort { token } => {
            put_u8(bytes, 7);
            put_u64(bytes, *token);
        }
    }
    Ok(())
}

fn encode_mutation(bytes: &mut Vec<u8>, mutation: &Mutation) -> Result<(), DaemonError> {
    match mutation {
        Mutation::SyncAll => put_u8(bytes, 1),
        Mutation::SyncCollection(collection_id) => {
            put_u8(bytes, 2);
            put_u64(bytes, *collection_id);
        }
        Mutation::Verify { full } => {
            put_u8(bytes, 3);
            put_u8(bytes, u8::from(*full));
        }
        Mutation::Compact => put_u8(bytes, 4),
        Mutation::Extension { name, payload } => {
            put_u8(bytes, 5);
            put_string(bytes, name)?;
            put_bytes(bytes, payload, MAX_MUTATION_PAYLOAD_BYTES)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
enum DecodeRequestError {
    UnsupportedVersion {
        request_id: u64,
        version: u16,
    },
    Invalid {
        request_id: Option<u64>,
        message: &'static str,
    },
}

fn decode_request(bytes: &[u8]) -> Result<Request, DecodeRequestError> {
    let mut decoder = Decoder::new(bytes);
    decoder
        .magic(REQUEST_MAGIC)
        .map_err(|message| DecodeRequestError::Invalid {
            request_id: None,
            message,
        })?;
    let version = decoder
        .u16()
        .map_err(|message| DecodeRequestError::Invalid {
            request_id: None,
            message,
        })?;
    let request_id = decoder
        .u64()
        .map_err(|message| DecodeRequestError::Invalid {
            request_id: None,
            message,
        })?;
    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
        return Err(DecodeRequestError::UnsupportedVersion {
            request_id,
            version,
        });
    }
    let tag = decoder
        .u8()
        .map_err(|message| DecodeRequestError::Invalid {
            request_id: Some(request_id),
            message,
        })?;
    let kind = match tag {
        1 => RequestKind::Health,
        2 => RequestKind::Status,
        3 => RequestKind::Mutate(decode_mutation(&mut decoder).map_err(|message| {
            DecodeRequestError::Invalid {
                request_id: Some(request_id),
                message,
            }
        })?),
        4 => RequestKind::Shutdown,
        5 if version >= 2 => {
            RequestKind::CollectionAdmin(decode_collection_admin_request(&mut decoder).map_err(
                |message| DecodeRequestError::Invalid {
                    request_id: Some(request_id),
                    message,
                },
            )?)
        }
        6 if version >= 2 => {
            RequestKind::SourceAdmin(decode_source_administration(&mut decoder).map_err(
                |message| DecodeRequestError::Invalid {
                    request_id: Some(request_id),
                    message,
                },
            )?)
        }
        _ => {
            return Err(DecodeRequestError::Invalid {
                request_id: Some(request_id),
                message: "unknown request kind",
            });
        }
    };
    decoder
        .finish()
        .map_err(|message| DecodeRequestError::Invalid {
            request_id: Some(request_id),
            message,
        })?;
    Ok(Request {
        protocol_version: version,
        request_id,
        kind,
    })
}

fn decode_source_administration(
    decoder: &mut Decoder<'_>,
) -> Result<SourceAdministration, &'static str> {
    match decoder.u8()? {
        1 => Ok(SourceAdministration::Add {
            api_endpoint: decoder.string(MAX_SOURCE_API_ENDPOINT_BYTES)?,
            language_code: decoder.string(MAX_SOURCE_LANGUAGE_CODE_BYTES)?,
        }),
        2 => Ok(SourceAdministration::Remove {
            wiki_id: wikisync_core::WikiId::new(decoder.u64()?)
                .map_err(|_| "invalid wiki ID in source administration request")?,
        }),
        _ => Err("unknown source administration operation"),
    }
}

fn decode_collection_admin_request(
    decoder: &mut Decoder<'_>,
) -> Result<CollectionAdminRequest, &'static str> {
    match decoder.u8()? {
        1 => {
            let total_bytes = decoder.u32()?;
            if total_bytes == 0 || total_bytes as usize > MAX_COLLECTION_DRAFT_BYTES {
                Err("collection draft size is outside its bound")
            } else {
                Ok(CollectionAdminRequest::Begin { total_bytes })
            }
        }
        2 => Ok(CollectionAdminRequest::Append {
            token: decoder.u64()?,
            offset: decoder.u32()?,
            bytes: decoder.bytes(MAX_COLLECTION_DRAFT_CHUNK_BYTES)?,
        }),
        3 => Ok(CollectionAdminRequest::Estimate {
            token: decoder.u64()?,
        }),
        4 => Ok(CollectionAdminRequest::Add {
            token: decoder.u64()?,
        }),
        5 => Ok(CollectionAdminRequest::Edit {
            token: decoder.u64()?,
            collection_id: decoder.u64()?,
            expected_generation: decoder.u64()?,
        }),
        6 => Ok(CollectionAdminRequest::Remove {
            collection_id: decoder.u64()?,
        }),
        7 => Ok(CollectionAdminRequest::Abort {
            token: decoder.u64()?,
        }),
        _ => Err("unknown collection administration operation"),
    }
}

fn decode_mutation(decoder: &mut Decoder<'_>) -> Result<Mutation, &'static str> {
    match decoder.u8()? {
        1 => Ok(Mutation::SyncAll),
        2 => Ok(Mutation::SyncCollection(decoder.u64()?)),
        3 => match decoder.u8()? {
            0 => Ok(Mutation::Verify { full: false }),
            1 => Ok(Mutation::Verify { full: true }),
            _ => Err("invalid verification coverage flag"),
        },
        4 => Ok(Mutation::Compact),
        5 => Ok(Mutation::Extension {
            name: decoder.string(MAX_STRING_BYTES)?,
            payload: decoder.bytes(MAX_MUTATION_PAYLOAD_BYTES)?,
        }),
        _ => Err("unknown mutation kind"),
    }
}

fn encode_response(response: &Response) -> Result<Vec<u8>, DaemonError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(RESPONSE_MAGIC);
    put_u16(&mut bytes, response.protocol_version);
    put_u64(&mut bytes, response.request_id);
    match &response.kind {
        ResponseKind::Health(health) => {
            put_u8(&mut bytes, 1);
            put_string(&mut bytes, &health.daemon_version)?;
            put_u32(&mut bytes, health.process_id);
            put_u64(&mut bytes, health.uptime_seconds);
        }
        ResponseKind::Status(status) => {
            put_u8(&mut bytes, 2);
            put_u32(&mut bytes, status.process_id);
            put_u64(&mut bytes, status.uptime_seconds);
            put_u64(&mut bytes, status.completed_mutations);
            put_string(&mut bytes, &status.state)?;
            put_string(&mut bytes, &status.detail)?;
        }
        ResponseKind::Mutated(outcome) => {
            put_u8(&mut bytes, 3);
            put_string(&mut bytes, &outcome.result)?;
            put_bytes(&mut bytes, &outcome.payload, MAX_MUTATION_PAYLOAD_BYTES)?;
        }
        ResponseKind::ShutdownAccepted => put_u8(&mut bytes, 4),
        ResponseKind::CollectionAdmin(outcome) => {
            if response.protocol_version < 2 {
                return Err(DaemonError::Protocol(
                    "collection administration response requires protocol version 2",
                ));
            }
            put_u8(&mut bytes, 5);
            encode_collection_admin_outcome(&mut bytes, outcome);
        }
        ResponseKind::SourceAdmin(outcome) => {
            if response.protocol_version < 2 {
                return Err(DaemonError::Protocol(
                    "source administration response requires protocol version 2",
                ));
            }
            put_u8(&mut bytes, 6);
            encode_source_administration_outcome(&mut bytes, outcome)?;
        }
        ResponseKind::Error(error) => {
            put_u8(&mut bytes, 255);
            put_u8(&mut bytes, error_code_tag(error.code));
            put_string(&mut bytes, &error.message)?;
        }
    }
    ensure_frame_size(bytes)
}

fn encode_source_administration_outcome(
    bytes: &mut Vec<u8>,
    outcome: &SourceAdministrationOutcome,
) -> Result<(), DaemonError> {
    match outcome {
        SourceAdministrationOutcome::Added {
            wiki_id,
            api_endpoint,
            language_code,
            created,
        } => {
            put_u8(bytes, 1);
            put_u64(bytes, wiki_id.get());
            put_bytes(
                bytes,
                api_endpoint.as_bytes(),
                MAX_SOURCE_API_ENDPOINT_BYTES,
            )?;
            put_bytes(
                bytes,
                language_code.as_bytes(),
                MAX_SOURCE_LANGUAGE_CODE_BYTES,
            )?;
            put_u8(bytes, u8::from(*created));
        }
        SourceAdministrationOutcome::Removed { wiki_id } => {
            put_u8(bytes, 2);
            put_u64(bytes, wiki_id.get());
        }
    }
    Ok(())
}

fn encode_collection_admin_outcome(bytes: &mut Vec<u8>, outcome: &CollectionAdminProtocolOutcome) {
    match outcome {
        CollectionAdminProtocolOutcome::Begun { token } => {
            put_u8(bytes, 1);
            put_u64(bytes, *token);
        }
        CollectionAdminProtocolOutcome::Appended {
            token,
            received_bytes,
            total_bytes,
        } => {
            put_u8(bytes, 2);
            put_u64(bytes, *token);
            put_u32(bytes, *received_bytes);
            put_u32(bytes, *total_bytes);
        }
        CollectionAdminProtocolOutcome::Aborted { token } => {
            put_u8(bytes, 3);
            put_u64(bytes, *token);
        }
        CollectionAdminProtocolOutcome::Completed(outcome) => {
            put_u8(bytes, 4);
            encode_collection_administration_outcome(bytes, outcome);
        }
    }
}

fn encode_collection_administration_outcome(
    bytes: &mut Vec<u8>,
    outcome: &CollectionAdministrationOutcome,
) {
    match outcome {
        CollectionAdministrationOutcome::Estimated(estimate) => {
            put_u8(bytes, 1);
            encode_collection_estimate(bytes, *estimate);
        }
        CollectionAdministrationOutcome::Added {
            collection_id,
            estimate,
        } => {
            put_u8(bytes, 2);
            put_u64(bytes, collection_id.get());
            encode_collection_estimate(bytes, *estimate);
        }
        CollectionAdministrationOutcome::Edited {
            collection_id,
            estimate,
        } => {
            put_u8(bytes, 3);
            put_u64(bytes, collection_id.get());
            encode_collection_estimate(bytes, *estimate);
        }
        CollectionAdministrationOutcome::Removed { collection_id } => {
            put_u8(bytes, 4);
            put_u64(bytes, collection_id.get());
        }
    }
}

fn encode_collection_estimate(bytes: &mut Vec<u8>, estimate: CollectionDraftEstimate) {
    put_u64(bytes, estimate.resolved_page_count);
    put_u64(bytes, estimate.missing_title_count);
    match estimate.predicted_canonical_bytes {
        Some(value) => {
            put_u8(bytes, 1);
            put_u64(bytes, value);
        }
        None => put_u8(bytes, 0),
    }
    put_u64(bytes, estimate.category_batches);
    put_u8(bytes, u8::from(estimate.fits_budget));
}

fn decode_response(bytes: &[u8]) -> Result<Response, DaemonError> {
    let mut decoder = Decoder::new(bytes);
    decoder
        .magic(RESPONSE_MAGIC)
        .map_err(DaemonError::Protocol)?;
    let version = decoder.u16().map_err(DaemonError::Protocol)?;
    let request_id = decoder.u64().map_err(DaemonError::Protocol)?;
    let kind = match decoder.u8().map_err(DaemonError::Protocol)? {
        1 => ResponseKind::Health(Health {
            daemon_version: decoder
                .string(MAX_STRING_BYTES)
                .map_err(DaemonError::Protocol)?,
            process_id: decoder.u32().map_err(DaemonError::Protocol)?,
            uptime_seconds: decoder.u64().map_err(DaemonError::Protocol)?,
        }),
        2 => ResponseKind::Status(DaemonStatus {
            process_id: decoder.u32().map_err(DaemonError::Protocol)?,
            uptime_seconds: decoder.u64().map_err(DaemonError::Protocol)?,
            completed_mutations: decoder.u64().map_err(DaemonError::Protocol)?,
            state: decoder
                .string(MAX_STRING_BYTES)
                .map_err(DaemonError::Protocol)?,
            detail: decoder
                .string(MAX_STRING_BYTES)
                .map_err(DaemonError::Protocol)?,
        }),
        3 => ResponseKind::Mutated(MutationOutcome {
            result: decoder
                .string(MAX_STRING_BYTES)
                .map_err(DaemonError::Protocol)?,
            payload: decoder
                .bytes(MAX_MUTATION_PAYLOAD_BYTES)
                .map_err(DaemonError::Protocol)?,
        }),
        4 => ResponseKind::ShutdownAccepted,
        5 if version >= 2 => {
            ResponseKind::CollectionAdmin(decode_collection_admin_outcome(&mut decoder)?)
        }
        6 if version >= 2 => {
            ResponseKind::SourceAdmin(decode_source_administration_outcome(&mut decoder)?)
        }
        255 => ResponseKind::Error(ResponseError {
            code: decode_error_code(decoder.u8().map_err(DaemonError::Protocol)?)?,
            message: decoder
                .string(MAX_STRING_BYTES)
                .map_err(DaemonError::Protocol)?,
        }),
        _ => return Err(DaemonError::Protocol("unknown response kind")),
    };
    decoder.finish().map_err(DaemonError::Protocol)?;
    Ok(Response {
        protocol_version: version,
        request_id,
        kind,
    })
}

fn decode_source_administration_outcome(
    decoder: &mut Decoder<'_>,
) -> Result<SourceAdministrationOutcome, DaemonError> {
    match decoder.u8().map_err(DaemonError::Protocol)? {
        1 => {
            let wiki_id = decode_wiki_id(decoder)?;
            let api_endpoint = decoder
                .string(MAX_SOURCE_API_ENDPOINT_BYTES)
                .map_err(DaemonError::Protocol)?;
            let language_code = decoder
                .string(MAX_SOURCE_LANGUAGE_CODE_BYTES)
                .map_err(DaemonError::Protocol)?;
            let created = match decoder.u8().map_err(DaemonError::Protocol)? {
                0 => false,
                1 => true,
                _ => return Err(DaemonError::Protocol("invalid source-created flag")),
            };
            Ok(SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint,
                language_code,
                created,
            })
        }
        2 => Ok(SourceAdministrationOutcome::Removed {
            wiki_id: decode_wiki_id(decoder)?,
        }),
        _ => Err(DaemonError::Protocol(
            "unknown source administration outcome",
        )),
    }
}

fn decode_wiki_id(decoder: &mut Decoder<'_>) -> Result<wikisync_core::WikiId, DaemonError> {
    wikisync_core::WikiId::new(decoder.u64().map_err(DaemonError::Protocol)?)
        .map_err(|_| DaemonError::Protocol("invalid wiki ID in response"))
}

fn decode_collection_admin_outcome(
    decoder: &mut Decoder<'_>,
) -> Result<CollectionAdminProtocolOutcome, DaemonError> {
    match decoder.u8().map_err(DaemonError::Protocol)? {
        1 => Ok(CollectionAdminProtocolOutcome::Begun {
            token: decoder.u64().map_err(DaemonError::Protocol)?,
        }),
        2 => Ok(CollectionAdminProtocolOutcome::Appended {
            token: decoder.u64().map_err(DaemonError::Protocol)?,
            received_bytes: decoder.u32().map_err(DaemonError::Protocol)?,
            total_bytes: decoder.u32().map_err(DaemonError::Protocol)?,
        }),
        3 => Ok(CollectionAdminProtocolOutcome::Aborted {
            token: decoder.u64().map_err(DaemonError::Protocol)?,
        }),
        4 => Ok(CollectionAdminProtocolOutcome::Completed(
            decode_collection_administration_outcome(decoder)?,
        )),
        _ => Err(DaemonError::Protocol(
            "unknown collection administration response",
        )),
    }
}

fn decode_collection_administration_outcome(
    decoder: &mut Decoder<'_>,
) -> Result<CollectionAdministrationOutcome, DaemonError> {
    match decoder.u8().map_err(DaemonError::Protocol)? {
        1 => Ok(CollectionAdministrationOutcome::Estimated(
            decode_collection_estimate(decoder)?,
        )),
        2 => Ok(CollectionAdministrationOutcome::Added {
            collection_id: decode_collection_id(decoder)?,
            estimate: decode_collection_estimate(decoder)?,
        }),
        3 => Ok(CollectionAdministrationOutcome::Edited {
            collection_id: decode_collection_id(decoder)?,
            estimate: decode_collection_estimate(decoder)?,
        }),
        4 => Ok(CollectionAdministrationOutcome::Removed {
            collection_id: decode_collection_id(decoder)?,
        }),
        _ => Err(DaemonError::Protocol(
            "unknown completed collection administration outcome",
        )),
    }
}

fn decode_collection_id(
    decoder: &mut Decoder<'_>,
) -> Result<wikisync_core::CollectionId, DaemonError> {
    wikisync_core::CollectionId::new(decoder.u64().map_err(DaemonError::Protocol)?)
        .map_err(|_| DaemonError::Protocol("invalid collection ID in response"))
}

fn decode_collection_estimate(
    decoder: &mut Decoder<'_>,
) -> Result<CollectionDraftEstimate, DaemonError> {
    let resolved_page_count = decoder.u64().map_err(DaemonError::Protocol)?;
    let missing_title_count = decoder.u64().map_err(DaemonError::Protocol)?;
    let predicted_canonical_bytes = match decoder.u8().map_err(DaemonError::Protocol)? {
        0 => None,
        1 => Some(decoder.u64().map_err(DaemonError::Protocol)?),
        _ => return Err(DaemonError::Protocol("invalid optional estimate encoding")),
    };
    let category_batches = decoder.u64().map_err(DaemonError::Protocol)?;
    let fits_budget = match decoder.u8().map_err(DaemonError::Protocol)? {
        0 => false,
        1 => true,
        _ => return Err(DaemonError::Protocol("invalid estimate budget flag")),
    };
    Ok(CollectionDraftEstimate {
        resolved_page_count,
        missing_title_count,
        predicted_canonical_bytes,
        category_batches,
        fits_budget,
    })
}

fn error_code_tag(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::InvalidRequest => 1,
        ErrorCode::UnsupportedVersion => 2,
        ErrorCode::Unsupported => 3,
        ErrorCode::OperationFailed => 4,
        ErrorCode::Internal => 5,
    }
}

fn decode_error_code(tag: u8) -> Result<ErrorCode, DaemonError> {
    match tag {
        1 => Ok(ErrorCode::InvalidRequest),
        2 => Ok(ErrorCode::UnsupportedVersion),
        3 => Ok(ErrorCode::Unsupported),
        4 => Ok(ErrorCode::OperationFailed),
        5 => Ok(ErrorCode::Internal),
        _ => Err(DaemonError::Protocol("unknown response error code")),
    }
}

fn ensure_frame_size(bytes: Vec<u8>) -> Result<Vec<u8>, DaemonError> {
    if bytes.len() > MAX_FRAME_BYTES {
        Err(DaemonError::FrameTooLarge { size: bytes.len() })
    } else {
        Ok(bytes)
    }
}

fn put_u8(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), DaemonError> {
    put_bytes(bytes, value.as_bytes(), MAX_STRING_BYTES)
}

fn put_bytes(bytes: &mut Vec<u8>, value: &[u8], maximum: usize) -> Result<(), DaemonError> {
    if value.len() > maximum {
        return Err(DaemonError::FrameTooLarge { size: value.len() });
    }
    let length =
        u32::try_from(value.len()).map_err(|_| DaemonError::FrameTooLarge { size: value.len() })?;
    put_u32(bytes, length);
    bytes.extend_from_slice(value);
    Ok(())
}

#[derive(Debug)]
struct Decoder<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
        }
    }

    fn magic(&mut self, expected: &[u8; 4]) -> Result<(), &'static str> {
        let mut actual = [0; 4];
        self.cursor
            .read_exact(&mut actual)
            .map_err(|_| "truncated envelope")?;
        if &actual == expected {
            Ok(())
        } else {
            Err("invalid envelope magic")
        }
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        let mut bytes = [0; 1];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| "truncated integer")?;
        Ok(bytes[0])
    }

    fn u16(&mut self) -> Result<u16, &'static str> {
        let mut bytes = [0; 2];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| "truncated integer")?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        let mut bytes = [0; 4];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| "truncated integer")?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        let mut bytes = [0; 8];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| "truncated integer")?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, &'static str> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err("bounded field is too large");
        }
        let mut bytes = vec![0; length];
        self.cursor
            .read_exact(&mut bytes)
            .map_err(|_| "truncated byte field")?;
        Ok(bytes)
    }

    fn string(&mut self, maximum: usize) -> Result<String, &'static str> {
        String::from_utf8(self.bytes(maximum)?).map_err(|_| "string field is not UTF-8")
    }

    fn finish(&self) -> Result<(), &'static str> {
        if self.cursor.position() == self.cursor.get_ref().len() as u64 {
            Ok(())
        } else {
            Err("trailing envelope bytes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug)]
    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            // Keep fixture paths inside the sandbox and below macOS's small
            // `sockaddr_un` limit. Process IDs make leftovers from another run
            // distinct; the atomic counter distinguishes this run's fixtures.
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-{}-{unique}", std::process::id()));
            fs::create_dir(&path).expect("create temporary library");
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

    #[derive(Debug, Default)]
    struct RecordingHandler {
        mutations: Arc<Mutex<Vec<Mutation>>>,
    }

    impl RequestHandler for RecordingHandler {
        fn status(&self) -> HandlerStatus {
            HandlerStatus {
                state: "idle".to_owned(),
                detail: "fixture handler".to_owned(),
            }
        }

        fn mutate(
            &mut self,
            mutation: Mutation,
            _control: OperationControl,
        ) -> Result<MutationOutcome, OperationError> {
            self.mutations.lock().expect("mutation lock").push(mutation);
            Ok(MutationOutcome {
                result: "completed".to_owned(),
                payload: b"fixture receipt".to_vec(),
            })
        }
    }

    #[derive(Debug)]
    struct CooperativeCancellationHandler {
        started: std::sync::mpsc::Sender<()>,
    }

    impl RequestHandler for CooperativeCancellationHandler {
        fn status(&self) -> HandlerStatus {
            HandlerStatus {
                state: "running".to_owned(),
                detail: "cooperative cancellation fixture".to_owned(),
            }
        }

        fn mutate(
            &mut self,
            _mutation: Mutation,
            control: OperationControl,
        ) -> Result<MutationOutcome, OperationError> {
            self.started.send(()).expect("announce mutation start");
            let deadline = Instant::now() + Duration::from_secs(2);
            while !control.is_shutdown_requested() {
                assert!(Instant::now() < deadline, "shutdown did not reach handler");
                thread::sleep(Duration::from_millis(1));
            }
            Err(OperationError::failed(
                "operation interrupted by daemon shutdown",
            ))
        }
    }

    fn running_daemon<H: RequestHandler>(
        library: &TempLibrary,
        handler: H,
    ) -> (Client, JoinHandle<Result<(), DaemonError>>) {
        let daemon = Daemon::bind(library.path(), handler).expect("bind daemon");
        let client = Client::for_library(library.path()).expect("client");
        let thread = thread::spawn(move || daemon.run());
        client
            .health()
            .expect("daemon must answer a readiness probe before the test continues");
        (client, thread)
    }

    fn collection_draft(wiki_id: wikisync_core::WikiId, name: &str) -> CollectionDraft {
        use wikisync_core::{
            CollectionBudget, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
            InclusionReason, PageId, PageTitle, TitleSelection,
        };
        let title = PageTitle::new("Rust").expect("title");
        CollectionDraft {
            wiki_id,
            name: name.to_owned(),
            preview: wikisync_sync::CollectionSelectionPreview {
                rule: CollectionRule::ExplicitTitles(
                    TitleSelection::new([title.clone()]).expect("selection"),
                ),
                members: vec![wikisync_store::ResolvedCollectionMember {
                    page_id: PageId::new(10).expect("page ID"),
                    namespace: 0,
                    title: title.clone(),
                    inclusion_reason: InclusionReason::ExplicitTitle(title),
                }],
                missing_titles: Vec::new(),
                predicted_canonical_bytes: Some(1_024),
                category_batches: 0,
            },
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
        }
    }

    fn multi_chunk_collection_draft(wiki_id: wikisync_core::WikiId) -> CollectionDraft {
        use wikisync_core::{
            CollectionBudget, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
            InclusionReason, PageId, PageTitle,
        };
        let category = PageTitle::new("Category:Systems").expect("category");
        let members = (1..=1_500_u64)
            .map(|raw_id| {
                let title =
                    PageTitle::new(format!("System article {raw_id:04}")).expect("generated title");
                wikisync_store::ResolvedCollectionMember {
                    page_id: PageId::new(raw_id).expect("page ID"),
                    namespace: 0,
                    title,
                    inclusion_reason: InclusionReason::Category {
                        category: category.clone(),
                        depth: 1,
                    },
                }
            })
            .collect();
        CollectionDraft {
            wiki_id,
            name: "Large preview".to_owned(),
            preview: wikisync_sync::CollectionSelectionPreview {
                rule: CollectionRule::Category {
                    title: category,
                    recursion_depth: 1,
                },
                members,
                missing_titles: Vec::new(),
                predicted_canonical_bytes: None,
                category_batches: 20,
            },
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
        }
    }

    #[test]
    fn protocol_round_trips_all_current_request_shapes() {
        let requests = vec![
            RequestKind::Health,
            RequestKind::Status,
            RequestKind::Mutate(Mutation::SyncAll),
            RequestKind::Mutate(Mutation::SyncCollection(42)),
            RequestKind::Mutate(Mutation::Verify { full: true }),
            RequestKind::Mutate(Mutation::Compact),
            RequestKind::Mutate(Mutation::Extension {
                name: "fixture".to_owned(),
                payload: b"bounded bytes".to_vec(),
            }),
            RequestKind::Shutdown,
            RequestKind::CollectionAdmin(CollectionAdminRequest::Begin { total_bytes: 99 }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Append {
                token: 7,
                offset: 48,
                bytes: b"chunk".to_vec(),
            }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Estimate { token: 7 }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Add { token: 7 }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Edit {
                token: 7,
                collection_id: 42,
                expected_generation: 9,
            }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Remove { collection_id: 42 }),
            RequestKind::CollectionAdmin(CollectionAdminRequest::Abort { token: 7 }),
            RequestKind::SourceAdmin(SourceAdministration::Add {
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "en".to_owned(),
            }),
            RequestKind::SourceAdmin(SourceAdministration::Remove {
                wiki_id: wikisync_core::WikiId::new(3).unwrap(),
            }),
        ];
        for (index, kind) in requests.into_iter().enumerate() {
            let request = Request {
                protocol_version: PROTOCOL_VERSION,
                request_id: index as u64 + 1,
                kind,
            };
            let encoded = encode_request(&request).expect("encode request");
            assert_eq!(decode_request(&encoded).expect("decode request"), request);
        }
    }

    #[test]
    fn protocol_one_requests_remain_compatible_but_cannot_use_v2_administration() {
        for kind in [
            RequestKind::Health,
            RequestKind::Status,
            RequestKind::Mutate(Mutation::SyncCollection(42)),
            RequestKind::Shutdown,
        ] {
            let request = Request {
                protocol_version: 1,
                request_id: 9,
                kind,
            };
            assert_eq!(
                decode_request(&encode_request(&request).expect("encode v1")).expect("decode v1"),
                request
            );
        }
        let v1_admin = Request {
            protocol_version: 1,
            request_id: 10,
            kind: RequestKind::CollectionAdmin(CollectionAdminRequest::Remove {
                collection_id: 42,
            }),
        };
        assert!(matches!(
            encode_request(&v1_admin),
            Err(DaemonError::Protocol(_))
        ));
        let v1_source_admin = Request {
            protocol_version: 1,
            request_id: 11,
            kind: RequestKind::SourceAdmin(SourceAdministration::Add {
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "en".to_owned(),
            }),
        };
        assert!(matches!(
            encode_request(&v1_source_admin),
            Err(DaemonError::Protocol(_))
        ));
    }

    #[test]
    fn protocol_round_trips_collection_administration_responses() {
        let estimate = CollectionDraftEstimate {
            resolved_page_count: 10_000,
            missing_title_count: 3,
            predicted_canonical_bytes: Some(5_000_000),
            category_batches: 20,
            fits_budget: true,
        };
        let outcomes = vec![
            CollectionAdminProtocolOutcome::Begun { token: 1 },
            CollectionAdminProtocolOutcome::Appended {
                token: 1,
                received_bytes: 48,
                total_bytes: 96,
            },
            CollectionAdminProtocolOutcome::Aborted { token: 1 },
            CollectionAdminProtocolOutcome::Completed(CollectionAdministrationOutcome::Estimated(
                estimate,
            )),
            CollectionAdminProtocolOutcome::Completed(CollectionAdministrationOutcome::Added {
                collection_id: wikisync_core::CollectionId::new(2).unwrap(),
                estimate,
            }),
            CollectionAdminProtocolOutcome::Completed(CollectionAdministrationOutcome::Edited {
                collection_id: wikisync_core::CollectionId::new(2).unwrap(),
                estimate,
            }),
            CollectionAdminProtocolOutcome::Completed(CollectionAdministrationOutcome::Removed {
                collection_id: wikisync_core::CollectionId::new(2).unwrap(),
            }),
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let response = Response {
                protocol_version: PROTOCOL_VERSION,
                request_id: index as u64 + 1,
                kind: ResponseKind::CollectionAdmin(outcome),
            };
            assert_eq!(
                decode_response(&encode_response(&response).expect("encode response"))
                    .expect("decode response"),
                response
            );
        }
    }

    #[test]
    fn protocol_round_trips_source_administration_responses() {
        let wiki_id = wikisync_core::WikiId::new(7).unwrap();
        for (index, outcome) in [
            SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "en".to_owned(),
                created: true,
            },
            SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "en".to_owned(),
                created: false,
            },
            SourceAdministrationOutcome::Removed { wiki_id },
        ]
        .into_iter()
        .enumerate()
        {
            let response = Response {
                protocol_version: PROTOCOL_VERSION,
                request_id: index as u64 + 1,
                kind: ResponseKind::SourceAdmin(outcome),
            };
            assert_eq!(
                decode_response(&encode_response(&response).expect("encode response"))
                    .expect("decode response"),
                response
            );
        }
    }

    #[test]
    fn source_administration_codec_enforces_field_bounds() {
        let oversized_endpoint = Request {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            kind: RequestKind::SourceAdmin(SourceAdministration::Add {
                api_endpoint: "x".repeat(MAX_SOURCE_API_ENDPOINT_BYTES + 1),
                language_code: "en".to_owned(),
            }),
        };
        assert!(matches!(
            encode_request(&oversized_endpoint),
            Err(DaemonError::FrameTooLarge { .. })
        ));

        let oversized_language = Request {
            protocol_version: PROTOCOL_VERSION,
            request_id: 2,
            kind: RequestKind::SourceAdmin(SourceAdministration::Add {
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "x".repeat(MAX_SOURCE_LANGUAGE_CODE_BYTES + 1),
            }),
        };
        assert!(matches!(
            encode_request(&oversized_language),
            Err(DaemonError::FrameTooLarge { .. })
        ));

        let mut declared_oversized = Vec::new();
        declared_oversized.extend_from_slice(REQUEST_MAGIC);
        put_u16(&mut declared_oversized, PROTOCOL_VERSION);
        put_u64(&mut declared_oversized, 3);
        put_u8(&mut declared_oversized, 6);
        put_u8(&mut declared_oversized, 1);
        put_u32(
            &mut declared_oversized,
            u32::try_from(MAX_SOURCE_API_ENDPOINT_BYTES + 1).unwrap(),
        );
        assert!(matches!(
            decode_request(&declared_oversized),
            Err(DecodeRequestError::Invalid {
                request_id: Some(3),
                message: "bounded field is too large",
            })
        ));
    }

    #[test]
    fn operation_control_tracks_process_local_shutdown_across_clones() {
        assert!(!OperationControl::running().is_shutdown_requested());

        let running = Arc::new(AtomicBool::new(true));
        let control = OperationControl {
            running: Arc::clone(&running),
        };
        let clone = control.clone();
        let shutdown = ShutdownHandle { running };

        assert!(!control.is_shutdown_requested());
        assert!(!clone.is_shutdown_requested());
        shutdown.shutdown();
        assert!(control.is_shutdown_requested());
        assert!(clone.is_shutdown_requested());
    }

    #[test]
    fn active_interrupted_mutation_is_not_counted_as_completed() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let control = OperationControl {
            running: Arc::clone(&running),
        };
        let shutdown = ShutdownHandle { running };
        let shutdown_thread = thread::spawn(move || {
            started_rx.recv().expect("mutation start");
            shutdown.shutdown();
        });
        let mut handler = CooperativeCancellationHandler {
            started: started_tx,
        };
        let mut completed_mutations = 0;

        let response = dispatch_mutation(
            &mut handler,
            Mutation::SyncAll,
            control,
            &mut completed_mutations,
        );
        shutdown_thread.join().expect("shutdown thread");

        assert!(matches!(
            response,
            ResponseKind::Error(ResponseError {
                code: ErrorCode::OperationFailed,
                ..
            })
        ));
        assert_eq!(completed_mutations, 0);
    }

    #[test]
    fn daemon_socket_is_private_and_writer_ownership_is_exclusive() {
        let library = TempLibrary::new();
        let daemon = Daemon::bind(library.path(), FoundationHandler).expect("first daemon");
        assert_eq!(
            fs::metadata(daemon_socket_path(library.path()))
                .expect("socket metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(writer_socket_path(library.path()))
                .expect("lease metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(matches!(
            Daemon::bind(library.path(), FoundationHandler),
            Err(DaemonError::WriterBusy)
        ));
        assert!(matches!(
            WriterLease::acquire(library.path()),
            Err(DaemonError::WriterBusy)
        ));
        drop(daemon);
        WriterLease::acquire(library.path()).expect("lease after daemon drop");
    }

    #[test]
    fn health_status_mutation_and_graceful_shutdown_use_real_ipc() {
        let library = TempLibrary::new();
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let handler = RecordingHandler {
            mutations: Arc::clone(&mutations),
        };
        let (client, daemon) = running_daemon(&library, handler);

        let health = client.health().expect("health");
        assert_eq!(health.process_id, std::process::id());
        let status = client.status().expect("status");
        assert_eq!(status.state, "idle");
        assert_eq!(status.completed_mutations, 0);
        let outcome = client
            .forward_mutation(Mutation::SyncCollection(7))
            .expect("forward mutation");
        assert_eq!(outcome.result, "completed");
        assert_eq!(
            mutations.lock().expect("mutation lock").as_slice(),
            &[Mutation::SyncCollection(7)]
        );
        assert_eq!(
            client.status().expect("updated status").completed_mutations,
            1
        );
        client.shutdown().expect("request shutdown");
        daemon.join().expect("join daemon").expect("daemon result");
        assert!(!daemon_socket_path(library.path()).exists());
        WriterLease::acquire(library.path()).expect("writer released after shutdown");
    }

    #[test]
    fn running_v2_daemon_answers_a_legacy_v1_health_frame_in_v1() {
        let library = TempLibrary::new();
        let (client, daemon) = running_daemon(&library, RecordingHandler::default());
        let request = Request {
            protocol_version: 1,
            request_id: 91,
            kind: RequestKind::Health,
        };
        let mut stream = UnixStream::connect(daemon_socket_path(library.path())).expect("connect");
        write_frame(&mut stream, &encode_request(&request).expect("encode v1")).expect("send v1");
        let response = decode_response(&read_frame(&mut stream).expect("read v1")).expect("v1");
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.request_id, 91);
        assert!(matches!(response.kind, ResponseKind::Health(_)));

        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
    }

    #[test]
    fn collection_administration_forwards_the_same_typed_lifecycle_over_real_ipc() {
        let library = TempLibrary::new();
        let mut stored = wikisync_store::Library::open(library.path()).expect("library");
        let wiki_id = stored
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("source");
        drop(stored);
        let handler = ApplicationHandler::new(library.path()).expect("handler");
        let (client, daemon) = running_daemon(&library, handler);

        let large = multi_chunk_collection_draft(wiki_id);
        assert!(
            encode_collection_draft(&large)
                .expect("encode large preview")
                .len()
                > MAX_COLLECTION_DRAFT_CHUNK_BYTES
        );
        assert!(matches!(
            client
                .administer_collection(CollectionAdministration::Estimate(large))
                .expect("multi-chunk estimate"),
            CollectionAdministrationOutcome::Estimated(CollectionDraftEstimate {
                resolved_page_count: 1_500,
                ..
            })
        ));

        let estimated = client
            .administer_collection(CollectionAdministration::Estimate(collection_draft(
                wiki_id, "Systems",
            )))
            .expect("estimate");
        assert!(matches!(
            estimated,
            CollectionAdministrationOutcome::Estimated(CollectionDraftEstimate {
                resolved_page_count: 1,
                fits_budget: true,
                ..
            })
        ));
        let thumbnails = wikisync_core::ThumbnailPolicy::new(800, 12, 2 * 1024 * 1024)
            .expect("thumbnail policy");
        let added = client
            .administer_collection(CollectionAdministration::AddWithImagePolicy {
                draft: collection_draft(wiki_id, "Systems"),
                image_policy: wikisync_core::ImagePolicy::Thumbnails(thumbnails),
            })
            .expect("add");
        let CollectionAdministrationOutcome::Added { collection_id, .. } = added else {
            panic!("unexpected add outcome");
        };
        let edited = client
            .administer_collection(CollectionAdministration::EditWithImagePolicy {
                collection_id,
                expected_generation: 1,
                draft: collection_draft(wiki_id, "Programming systems"),
                image_policy: wikisync_core::ImagePolicy::None,
            })
            .expect("edit");
        assert!(matches!(
            edited,
            CollectionAdministrationOutcome::Edited {
                collection_id: edited_id,
                ..
            } if edited_id == collection_id
        ));
        let stale = client
            .administer_collection(CollectionAdministration::EditWithImagePolicy {
                collection_id,
                expected_generation: 1,
                draft: collection_draft(wiki_id, "Stale replacement"),
                image_policy: wikisync_core::ImagePolicy::Thumbnails(thumbnails),
            })
            .expect_err("stale forwarded preview must fail");
        assert!(matches!(
            stale,
            DaemonError::Remote(ResponseError {
                code: ErrorCode::OperationFailed,
                ref message,
            }) if message.contains("changed while it was being previewed")
        ));
        let stored = wikisync_store::Library::open_read_only(library.path()).expect("inspect");
        let configuration = stored
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(configuration.generation, 2);
        assert_eq!(configuration.name, "Programming systems");
        assert_eq!(configuration.image_policy, wikisync_core::ImagePolicy::None);
        drop(stored);
        client
            .administer_collection(CollectionAdministration::Remove { collection_id })
            .expect("remove");
        assert_eq!(client.status().expect("status").completed_mutations, 3);

        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
        let stored = wikisync_store::Library::open(library.path()).expect("reopen");
        let retained = stored
            .collection(collection_id)
            .expect("collection")
            .expect("retained tombstone");
        assert_eq!(retained.name, "Programming systems");
        assert_eq!(
            retained.status,
            wikisync_store::CollectionStatus::Tombstoned
        );
    }

    #[test]
    fn source_administration_preserves_idempotence_and_safe_removal_over_real_ipc() {
        let library = TempLibrary::new();
        let mut stored = wikisync_store::Library::open(library.path()).expect("library");
        let used_wiki_id = stored
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("used source");
        let used_collection_id = stored
            .create_explicit_collection(used_wiki_id, "Retained collection")
            .expect("collection");
        drop(stored);

        let handler = ApplicationHandler::new(library.path()).expect("handler");
        let (client, daemon) = running_daemon(&library, handler);
        let endpoint = "https://example.org/w/api.php";
        let added = client
            .administer_source(SourceAdministration::Add {
                api_endpoint: endpoint.to_owned(),
                language_code: "example".to_owned(),
            })
            .expect("add source");
        let SourceAdministrationOutcome::Added {
            wiki_id,
            created,
            language_code,
            ..
        } = added
        else {
            panic!("unexpected add outcome");
        };
        assert!(created);
        assert_eq!(language_code, "example");

        assert_eq!(
            client
                .administer_source(SourceAdministration::Add {
                    api_endpoint: endpoint.to_owned(),
                    language_code: "replacement".to_owned(),
                })
                .expect("repeat registration"),
            SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint: endpoint.to_owned(),
                language_code: "example".to_owned(),
                created: false,
            }
        );
        let removal_error = client
            .administer_source(SourceAdministration::Remove {
                wiki_id: used_wiki_id,
            })
            .expect_err("in-use source removal must fail");
        assert!(matches!(
            removal_error,
            DaemonError::Remote(ResponseError {
                code: ErrorCode::OperationFailed,
                ref message,
            }) if message.contains("still in use")
        ));
        assert_eq!(
            client
                .administer_source(SourceAdministration::Remove { wiki_id })
                .expect("remove unused source"),
            SourceAdministrationOutcome::Removed { wiki_id }
        );
        assert_eq!(client.status().expect("status").completed_mutations, 3);

        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
        let stored = wikisync_store::Library::open(library.path()).expect("reopen");
        assert!(stored.wiki(wiki_id).expect("removed source").is_none());
        assert!(stored.wiki(used_wiki_id).expect("used source").is_some());
        assert!(
            stored
                .collection(used_collection_id)
                .expect("used collection")
                .is_some()
        );
    }

    #[test]
    fn writer_access_selects_direct_or_daemon_without_opening_two_writers() {
        let library = TempLibrary::new();
        let direct = WriterAccess::discover(library.path()).expect("direct writer access");
        assert!(matches!(direct, WriterAccess::Direct(_)));
        assert!(matches!(
            WriterAccess::discover(library.path()),
            Err(DaemonError::WriterBusy)
        ));
        drop(direct);

        let (client, daemon) = running_daemon(&library, RecordingHandler::default());
        let access = WriterAccess::discover(library.path()).expect("daemon access");
        assert!(matches!(access, WriterAccess::Daemon(_)));
        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
    }

    #[test]
    fn stale_socket_is_recovered_but_unexpected_paths_are_never_removed() {
        let library = TempLibrary::new();
        let stale = UnixListener::bind(writer_socket_path(library.path())).expect("stale bind");
        drop(stale);
        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect stale writer"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::Stale,
            }
        );
        let lease = WriterLease::acquire(library.path()).expect("recover stale writer socket");
        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect recovered writer"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::Active,
            }
        );
        drop(lease);

        let other = TempLibrary::new();
        fs::write(writer_socket_path(other.path()), b"do not remove").expect("fixture file");
        assert!(matches!(
            WriterLease::acquire(other.path()),
            Err(DaemonError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            fs::read(writer_socket_path(other.path())).expect("fixture retained"),
            b"do not remove"
        );

        let symlink_library = TempLibrary::new();
        let target = symlink_library.path().join("target");
        fs::write(&target, b"do not follow").expect("symlink target");
        std::os::unix::fs::symlink(&target, writer_socket_path(symlink_library.path()))
            .expect("fixture symlink");
        assert_eq!(
            inspect_control_plane(symlink_library.path()).expect("inspect symlink"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::UnexpectedPath,
            }
        );
        assert!(matches!(
            WriterLease::acquire(symlink_library.path()),
            Err(DaemonError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert!(
            fs::symlink_metadata(writer_socket_path(symlink_library.path()))
                .expect("symlink retained")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).expect("target retained"), b"do not follow");
    }

    #[test]
    fn control_plane_inspection_is_read_only_when_the_lock_is_absent() {
        let library = TempLibrary::new();
        let entries_before = directory_entry_names(library.path());

        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect empty control plane"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::Missing,
            }
        );
        assert_eq!(directory_entry_names(library.path()), entries_before);
        assert!(!library.path().join(IPC_LOCK_NAME).exists());
    }

    #[test]
    fn concurrent_stale_recovery_selects_exactly_one_writer_owner() {
        let library = TempLibrary::new();
        let stale = UnixListener::bind(writer_socket_path(library.path())).expect("stale bind");
        drop(stale);
        let start = Arc::new(Barrier::new(3));
        let finished = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();

        for _ in 0..2 {
            let path = library.path().to_path_buf();
            let start = Arc::clone(&start);
            let finished = Arc::clone(&finished);
            threads.push(thread::spawn(move || {
                start.wait();
                let result = WriterLease::acquire(path);
                let outcome = match &result {
                    Ok(_) => "owner",
                    Err(DaemonError::WriterBusy) => "busy",
                    Err(_) => "unexpected-error",
                };
                finished.wait();
                drop(result);
                outcome
            }));
        }

        start.wait();
        finished.wait();
        let outcomes = threads
            .into_iter()
            .map(|thread| thread.join().expect("recovery thread"))
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == "owner")
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == "busy")
                .count(),
            1
        );
    }

    #[test]
    fn daemon_recovers_both_stale_sockets_and_creates_a_private_lock() {
        let library = TempLibrary::new();
        let stale_writer =
            UnixListener::bind(writer_socket_path(library.path())).expect("stale writer bind");
        let stale_daemon =
            UnixListener::bind(daemon_socket_path(library.path())).expect("stale daemon bind");
        drop((stale_writer, stale_daemon));

        let daemon = Daemon::bind(library.path(), FoundationHandler).expect("recover both sockets");
        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect recovered daemon"),
            ControlPlaneState {
                daemon: LocalSocketState::Active,
                writer: LocalSocketState::Active,
            }
        );
        let lock_metadata =
            fs::symlink_metadata(library.path().join(IPC_LOCK_NAME)).expect("IPC lock metadata");
        assert!(lock_metadata.file_type().is_file());
        assert_eq!(lock_metadata.permissions().mode() & 0o777, 0o600);

        drop(daemon);
        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect stopped daemon"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::Missing,
            }
        );
    }

    #[test]
    fn active_socket_is_preserved_when_an_owner_does_not_use_the_ipc_lock() {
        let library = TempLibrary::new();
        let path = writer_socket_path(library.path());
        let listener = UnixListener::bind(&path).expect("active fixture bind");
        let metadata = fs::symlink_metadata(&path).expect("active socket metadata");

        assert_eq!(
            inspect_control_plane(library.path()).expect("inspect active socket"),
            ControlPlaneState {
                daemon: LocalSocketState::Missing,
                writer: LocalSocketState::Active,
            }
        );
        assert!(matches!(
            WriterLease::acquire(library.path()),
            Err(DaemonError::WriterBusy)
        ));
        let retained = fs::symlink_metadata(&path).expect("active socket retained");
        assert_eq!(
            (retained.dev(), retained.ino()),
            (metadata.dev(), metadata.ino())
        );
        drop(listener);
    }

    fn directory_entry_names(path: &Path) -> Vec<std::ffi::OsString> {
        let mut entries = fs::read_dir(path)
            .expect("read fixture directory")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[test]
    fn oversized_frames_and_mutation_payloads_are_rejected_before_allocation_or_send() {
        let oversized = Mutation::Extension {
            name: "large".to_owned(),
            payload: vec![0; MAX_MUTATION_PAYLOAD_BYTES + 1],
        };
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            request_id: 1,
            kind: RequestKind::Mutate(oversized),
        };
        assert!(matches!(
            encode_request(&request),
            Err(DaemonError::FrameTooLarge { .. })
        ));

        let declared = u32::try_from(MAX_FRAME_BYTES + 1)
            .expect("frame bound fits u32")
            .to_be_bytes();
        assert!(matches!(
            read_frame(&mut Cursor::new(declared)),
            Err(DaemonError::FrameTooLarge { size }) if size == MAX_FRAME_BYTES + 1
        ));
    }

    #[test]
    fn foundation_never_claims_an_unimplemented_mutation_succeeded() {
        let library = TempLibrary::new();
        let (client, daemon) = running_daemon(&library, FoundationHandler);
        let result = client.forward_mutation(Mutation::SyncAll);
        assert!(
            matches!(
                result,
                Err(DaemonError::Remote(ResponseError {
                    code: ErrorCode::Unsupported,
                    ..
                }))
            ),
            "foundation returned an unexpected mutation result: {result:?}"
        );
        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
    }

    #[test]
    fn disconnected_client_does_not_terminate_daemon() {
        let library = TempLibrary::new();
        let (client, daemon) = running_daemon(&library, FoundationHandler);
        drop(UnixStream::connect(daemon_socket_path(library.path())).expect("connect then drop"));
        thread::sleep(SOCKET_POLL_INTERVAL * 2);
        client.health().expect("daemon remains healthy");
        client.shutdown().expect("shutdown");
        daemon.join().expect("join").expect("daemon result");
    }

    #[test]
    fn process_local_handle_gracefully_stops_the_accept_loop() {
        let library = TempLibrary::new();
        let daemon = Daemon::bind(library.path(), FoundationHandler).expect("bind daemon");
        let shutdown = daemon.shutdown_handle();
        let thread = thread::spawn(move || daemon.run());
        shutdown.shutdown();
        thread.join().expect("join").expect("daemon result");
        assert!(!daemon_socket_path(library.path()).exists());
        WriterLease::acquire(library.path()).expect("writer lease released");
    }

    #[test]
    fn oversized_multibyte_operation_errors_are_truncated_on_a_character_boundary() {
        let message = "a".repeat(MAX_STRING_BYTES - 1) + "🦀" + &"b".repeat(16);
        let bounded = bounded_message(message);
        assert!(bounded.len() <= MAX_STRING_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert_eq!(bounded, "a".repeat(MAX_STRING_BYTES - 1));
    }
}
