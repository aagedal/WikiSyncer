use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::{self, JoinHandle};

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use wikisyncd::{ApplicationHandler, Client, Daemon, ShutdownHandle};

const USAGE: &str = "WikiSyncer single-writer daemon

Usage:
  wikisyncd --library <path> [run]
  wikisyncd --library <path> health
  wikisyncd --library <path> status
  wikisyncd --library <path> shutdown
  wikisyncd --help
  wikisyncd --version

The WIKISYNC_LIBRARY environment variable may replace --library. `run` stays in the
foreground and stops gracefully on SIGINT, SIGTERM, or a `shutdown` request.
Synchronization, integrity verification, and compaction are available to versioned
IPC clients. Configured interval and daily-UTC schedules run automatically.";

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wikisyncd: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<(), CliError> {
    let mut arguments = arguments.into_iter();
    let mut library = None;
    let mut action = None;
    while let Some(argument) = arguments.next() {
        if argument == OsStr::new("--help") || argument == OsStr::new("-h") {
            println!("{USAGE}");
            return Ok(());
        }
        if argument == OsStr::new("--version") || argument == OsStr::new("-V") {
            println!("wikisyncd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        if argument == OsStr::new("--library") {
            let path = arguments
                .next()
                .ok_or_else(|| CliError::message("--library requires a path"))?;
            if library.replace(PathBuf::from(path)).is_some() {
                return Err(CliError::message("--library may only be supplied once"));
            }
            continue;
        }
        if action.replace(parse_action(&argument)?).is_some() {
            return Err(CliError::message("only one action may be supplied"));
        }
    }
    let library = library
        .or_else(|| env::var_os("WIKISYNC_LIBRARY").map(PathBuf::from))
        .ok_or_else(|| CliError::message("--library or WIKISYNC_LIBRARY is required"))?;
    if !library.join("library.sqlite3").is_file() {
        return Err(CliError::message(format!(
            "{} is not an initialized WikiSyncer library",
            library.display()
        )));
    }
    match action.unwrap_or(Action::Run) {
        Action::Run => {
            let handler = ApplicationHandler::new(&library)?;
            let daemon = Daemon::bind(&library, handler)?;
            let signal_monitor = SignalMonitor::install(daemon.shutdown_handle())?;
            println!("WikiSyncer daemon ready for {}", library.display());
            let daemon_result = daemon.run().map_err(Into::into);
            let signal_result = signal_monitor.finish();
            daemon_result.and(signal_result)
        }
        Action::Health => {
            let health = Client::for_library(&library)?.health()?;
            println!(
                "healthy: protocol {}, daemon {}, pid {}, uptime {}s",
                wikisyncd::PROTOCOL_VERSION,
                health.daemon_version,
                health.process_id,
                health.uptime_seconds
            );
            Ok(())
        }
        Action::Status => {
            let status = Client::for_library(&library)?.status()?;
            println!(
                "{}: {} (pid {}, uptime {}s, {} completed mutations)",
                status.state,
                status.detail,
                status.process_id,
                status.uptime_seconds,
                status.completed_mutations
            );
            Ok(())
        }
        Action::Shutdown => Client::for_library(&library)?
            .shutdown()
            .map_err(Into::into),
    }
}

struct SignalMonitor {
    handle: SignalHandle,
    thread: JoinHandle<()>,
}

impl SignalMonitor {
    fn install(shutdown: ShutdownHandle) -> Result<Self, CliError> {
        let mut signals = Signals::new([SIGINT, SIGTERM]).map_err(|error| {
            CliError::message(format!(
                "failed to install shutdown signal handlers: {error}"
            ))
        })?;
        let handle = signals.handle();
        let thread = thread::Builder::new()
            .name("wikisyncd-signals".to_owned())
            .spawn(move || {
                for _signal in signals.forever() {
                    shutdown.shutdown();
                }
            })
            .map_err(|error| {
                CliError::message(format!("failed to start shutdown signal monitor: {error}"))
            })?;
        Ok(Self { handle, thread })
    }

    fn finish(self) -> Result<(), CliError> {
        self.handle.close();
        self.thread
            .join()
            .map_err(|_| CliError::message("shutdown signal monitor panicked"))
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Run,
    Health,
    Status,
    Shutdown,
}

fn parse_action(value: &OsStr) -> Result<Action, CliError> {
    match value.to_str() {
        Some("run") => Ok(Action::Run),
        Some("health") => Ok(Action::Health),
        Some("status") => Ok(Action::Status),
        Some("shutdown") => Ok(Action::Shutdown),
        Some(value) => Err(CliError::message(format!("unknown action {value:?}"))),
        None => Err(CliError::message("action must be valid UTF-8")),
    }
}

#[derive(Debug)]
struct CliError(String);

impl CliError {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

impl From<wikisyncd::DaemonError> for CliError {
    fn from(error: wikisyncd::DaemonError) -> Self {
        Self(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use wikisyncd::{
        ErrorCode, HandlerStatus, Mutation, MutationOutcome, OperationControl, OperationError,
        RequestHandler, WriterLease, daemon_socket_path,
    };

    use super::*;

    static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn termination_signals_reach_active_operations_and_release_daemon_resources() {
        for signal in [SIGINT, SIGTERM] {
            let directory = TestDirectory::new();
            let (started_tx, started_rx) = mpsc::channel();
            let daemon = Daemon::bind(
                directory.path(),
                SignalAwareHandler {
                    started: started_tx,
                },
            )
            .expect("bind daemon");
            let signal_monitor =
                SignalMonitor::install(daemon.shutdown_handle()).expect("install signal handlers");
            let client = Client::for_library(directory.path()).expect("daemon client");
            let daemon_thread = thread::spawn(move || daemon.run());
            client.health().expect("daemon readiness");
            let signal_thread = thread::spawn(move || {
                started_rx.recv().expect("active mutation");
                signal_hook::low_level::raise(signal).expect("raise termination signal");
            });

            let mutation = client.forward_mutation(Mutation::SyncAll);
            assert!(matches!(
                mutation,
                Err(wikisyncd::DaemonError::Remote(error))
                    if error.code == ErrorCode::OperationFailed
            ));
            signal_thread.join().expect("signal thread");
            daemon_thread
                .join()
                .expect("daemon thread")
                .expect("graceful daemon stop");
            signal_monitor.finish().expect("finish signal monitor");

            assert!(!daemon_socket_path(directory.path()).exists());
            WriterLease::acquire(directory.path()).expect("writer lease released");
        }
    }

    #[derive(Debug)]
    struct SignalAwareHandler {
        started: mpsc::Sender<()>,
    }

    impl RequestHandler for SignalAwareHandler {
        fn status(&self) -> HandlerStatus {
            HandlerStatus {
                state: "running".to_owned(),
                detail: "signal fixture".to_owned(),
            }
        }

        fn mutate(
            &mut self,
            _mutation: Mutation,
            control: OperationControl,
        ) -> Result<MutationOutcome, OperationError> {
            self.started.send(()).expect("announce mutation start");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !control.is_shutdown_requested() {
                assert!(
                    Instant::now() < deadline,
                    "signal did not reach active handler"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(OperationError::failed(
                "operation interrupted by daemon shutdown",
            ))
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("wikisyncd-signal-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }
}
