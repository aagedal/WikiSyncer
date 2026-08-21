//! Local IPC and single-writer ownership for a WikiSyncer library.
//!
//! The daemon and short-lived direct writers cooperate through [`WriterLease`].
//! GUI and CLI callers should normally use [`WriterAccess::discover`] instead of
//! opening a writer directly: it forwards to a healthy daemon, acquires the lease
//! when no daemon is present, and reports a busy library in every other case.

mod application;

pub use application::ApplicationHandler;

use std::error::Error;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Current on-wire request and response contract version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Largest accepted request or response frame, excluding its four-byte length.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Largest opaque mutation payload accepted by the version-one contract.
pub const MAX_MUTATION_PAYLOAD_BYTES: usize = 60 * 1024;
/// Library-local daemon request socket name.
pub const DAEMON_SOCKET_NAME: &str = ".wikisyncd.sock";
/// Library-local cooperative writer lease socket name.
pub const WRITER_SOCKET_NAME: &str = ".wikisync-writer.sock";

const REQUEST_MAGIC: &[u8; 4] = b"WKSR";
const RESPONSE_MAGIC: &[u8; 4] = b"WKSP";
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(20);
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
    /// Structured rejection. Unsupported operations use [`ErrorCode::Unsupported`].
    Error(ResponseError),
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
    /// Produces read-only application status.
    fn status(&self) -> HandlerStatus;

    /// Performs one mutation synchronously under daemon writer ownership.
    /// Returning success must mean the requested work actually completed.
    fn mutate(&mut self, mutation: Mutation) -> Result<MutationOutcome, OperationError>;
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

    fn mutate(&mut self, _mutation: Mutation) -> Result<MutationOutcome, OperationError> {
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
        let path = writer_socket_path(root);
        let (listener, socket) = bind_exclusive(&path, BusyKind::Writer)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let monitor_stop = Arc::clone(&stop);
        let monitor = thread::Builder::new()
            .name("wikisync-writer-lease".to_owned())
            .spawn(move || {
                while !monitor_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((_stream, _address)) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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
        match self.call(RequestKind::Health)? {
            ResponseKind::Health(health) => Ok(health),
            kind => Err(unexpected_response("health", kind)),
        }
    }

    /// Requests read-only daemon and application status.
    pub fn status(&self) -> Result<DaemonStatus, DaemonError> {
        match self.call(RequestKind::Status)? {
            ResponseKind::Status(status) => Ok(status),
            kind => Err(unexpected_response("status", kind)),
        }
    }

    /// Forwards a mutation to the process holding daemon writer ownership.
    pub fn forward_mutation(&self, mutation: Mutation) -> Result<MutationOutcome, DaemonError> {
        match self.call(RequestKind::Mutate(mutation))? {
            ResponseKind::Mutated(outcome) => Ok(outcome),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("mutation", kind)),
        }
    }

    /// Requests graceful shutdown after the daemon's current request completes.
    pub fn shutdown(&self) -> Result<(), DaemonError> {
        match self.call(RequestKind::Shutdown)? {
            ResponseKind::ShutdownAccepted => Ok(()),
            ResponseKind::Error(error) => Err(DaemonError::Remote(error)),
            kind => Err(unexpected_response("shutdown", kind)),
        }
    }

    /// Sends any public request kind using the current protocol version.
    pub fn call(&self, kind: RequestKind) -> Result<ResponseKind, DaemonError> {
        let read_timeout = if matches!(kind, RequestKind::Mutate(_) | RequestKind::Shutdown) {
            LONG_OPERATION_TIMEOUT
        } else {
            CLIENT_IO_TIMEOUT
        };
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            kind,
        };
        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
        write_frame(&mut stream, &encode_request(&request)?)?;
        let response = decode_response(&read_frame(&mut stream)?)?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(DaemonError::Protocol("daemon response version changed"));
        }
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
}

impl<H: RequestHandler> Daemon<H> {
    /// Acquires writer ownership and binds private library-local IPC.
    pub fn bind(library_root: impl AsRef<Path>, handler: H) -> Result<Self, DaemonError> {
        let root = canonical_library_root(library_root.as_ref())?;
        let writer_lease = WriterLease::acquire(&root)?;
        let path = daemon_socket_path(&root);
        let (listener, socket) = bind_exclusive(&path, BusyKind::Daemon)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            _socket: socket,
            _writer_lease: writer_lease,
            handler,
            running: Arc::new(AtomicBool::new(true)),
            started: Instant::now(),
            completed_mutations: 0,
        })
    }

    /// Returns a process-local handle that requests shutdown between IPC requests.
    #[must_use]
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
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
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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
                        "protocol version {version} is unsupported; expected {PROTOCOL_VERSION}"
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
            RequestKind::Mutate(mutation) => match self.handler.mutate(mutation) {
                Ok(outcome)
                    if outcome.result.len() <= MAX_STRING_BYTES
                        && outcome.payload.len() <= MAX_MUTATION_PAYLOAD_BYTES =>
                {
                    self.completed_mutations = self.completed_mutations.saturating_add(1);
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
            },
            RequestKind::Shutdown => {
                self.running.store(false, Ordering::Release);
                ResponseKind::ShutdownAccepted
            }
        };
        Response {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            kind,
        }
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
    /// An inactive socket name exists and is not automatically removed because
    /// replacement cannot be made race-free with portable safe Unix APIs.
    StaleSocket(PathBuf),
    /// A bounded frame exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge { size: usize },
    /// Malformed or inconsistent protocol data.
    Protocol(&'static str),
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
                "stale socket {} must be inspected and removed before retrying",
                path.display()
            ),
            Self::FrameTooLarge { size } => write!(
                formatter,
                "IPC frame is {size} bytes; maximum is {MAX_FRAME_BYTES}"
            ),
            Self::Protocol(message) => write!(formatter, "invalid daemon protocol: {message}"),
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

fn bind_exclusive(path: &Path, busy: BusyKind) -> Result<(UnixListener, SocketGuard), DaemonError> {
    match UnixListener::bind(path) {
        Ok(listener) => {
            let guard = SocketGuard::new(path.to_path_buf())?;
            Ok((listener, guard))
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.file_type().is_socket() {
                return Err(DaemonError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("refusing to replace non-socket path {}", path.display()),
                )));
            }
            if UnixStream::connect(path).is_ok() {
                return Err(match busy {
                    BusyKind::Daemon => DaemonError::AlreadyRunning,
                    BusyKind::Writer => DaemonError::WriterBusy,
                });
            }
            // Do not unlink here. Between a failed connect and unlink, another
            // process could replace the path with its new live socket. Failing
            // closed makes stale recovery explicit and preserves exclusivity.
            Err(DaemonError::StaleSocket(path.to_path_buf()))
        }
        Err(error) => Err(error.into()),
    }
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
    }
    ensure_frame_size(bytes)
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
    if version != PROTOCOL_VERSION {
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
        ResponseKind::Error(error) => {
            put_u8(&mut bytes, 255);
            put_u8(&mut bytes, error_code_tag(error.code));
            put_string(&mut bytes, &error.message)?;
        }
    }
    ensure_frame_size(bytes)
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
    use std::sync::Mutex;

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

        fn mutate(&mut self, mutation: Mutation) -> Result<MutationOutcome, OperationError> {
            self.mutations.lock().expect("mutation lock").push(mutation);
            Ok(MutationOutcome {
                result: "completed".to_owned(),
                payload: b"fixture receipt".to_vec(),
            })
        }
    }

    fn running_daemon<H: RequestHandler>(
        library: &TempLibrary,
        handler: H,
    ) -> (Client, JoinHandle<Result<(), DaemonError>>) {
        let daemon = Daemon::bind(library.path(), handler).expect("bind daemon");
        let client = Client::for_library(library.path()).expect("client");
        let thread = thread::spawn(move || daemon.run());
        (client, thread)
    }

    #[test]
    fn protocol_round_trips_all_version_one_request_shapes() {
        let requests = [
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
    fn stale_and_non_socket_paths_fail_closed_and_are_never_removed() {
        let library = TempLibrary::new();
        let stale = UnixListener::bind(writer_socket_path(library.path())).expect("stale bind");
        drop(stale);
        assert!(matches!(
            WriterLease::acquire(library.path()),
            Err(DaemonError::StaleSocket(path)) if path == writer_socket_path(library.path())
        ));
        assert!(writer_socket_path(library.path()).exists());

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
        assert!(matches!(
            client.forward_mutation(Mutation::SyncAll),
            Err(DaemonError::Remote(ResponseError {
                code: ErrorCode::Unsupported,
                ..
            }))
        ));
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
