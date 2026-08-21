use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;

use wikisyncd::{ApplicationHandler, Client, Daemon};

const USAGE: &str = "WikiSyncer single-writer daemon

Usage:
  wikisyncd --library <path> [run]
  wikisyncd --library <path> health
  wikisyncd --library <path> status
  wikisyncd --library <path> shutdown
  wikisyncd --help
  wikisyncd --version

The WIKISYNC_LIBRARY environment variable may replace --library. `run` stays in the
foreground; service managers should use `shutdown` as their graceful stop command.
Synchronization, integrity verification, and compaction are available to versioned
IPC clients. Scheduling is not part of this build.";

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
            println!("WikiSyncer daemon ready for {}", library.display());
            daemon.run()?;
            Ok(())
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
