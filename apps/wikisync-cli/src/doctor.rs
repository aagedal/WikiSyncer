//! Offline, allowlisted diagnostics and redacted bundle generation.

use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};
use wikisync_core::PageTitle;
use wikisync_integrity::{VerificationCoverage, VerificationScope, verify_library};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient, RetryPolicy};
use wikisync_store::{Library, ScheduleCadence, SyncRunState};
use wikisyncd::{Client, LocalSocketState, application_user_agent, inspect_control_plane};

const BUNDLE_FORMAT: &str = "wikisync-doctor";
const BUNDLE_VERSION: u32 = 1;
const RECENT_RUN_LIMIT: u32 = 20;
const REACHABILITY_SOURCE_LIMIT: usize = 20;
const REACHABILITY_RESPONSE_LIMIT: usize = 256 * 1024;
const REACHABILITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const REACHABILITY_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs doctor and optionally performs explicit online checks or creates a bundle.
pub(crate) fn run(
    library_root: &Path,
    json_output: bool,
    bundle: Option<&Path>,
    online: bool,
) -> Result<(), DoctorError> {
    let report = collect(library_root, online);
    if let Some(bundle) = bundle {
        write_bundle(bundle, &report)?;
    }
    if json_output {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        serde_json::to_writer_pretty(&mut output, &report)?;
        output.write_all(b"\n")?;
    } else {
        println!("{}", human_summary(&report));
        if bundle.is_some() {
            println!("Redacted diagnostic bundle created.");
        }
    }
    Ok(())
}

fn collect(library_root: &Path, online: bool) -> Value {
    let storage = storage_section(library_root);
    let control_plane = control_plane_section(library_root);
    let library = Library::open_read_only(library_root);
    let (catalog, recent_runs, verification, source_reachability) = match library {
        Ok(library) => (
            catalog_section(&library),
            recent_runs_section(&library),
            verification_section(&library),
            source_reachability_section(&library, online),
        ),
        Err(_) => (
            section_error("library-unavailable"),
            section_error("library-unavailable"),
            section_error("library-unavailable"),
            if online {
                section_error("library-unavailable")
            } else {
                reachability_not_requested()
            },
        ),
    };

    json!({
        "format": {
            "name": BUNDLE_FORMAT,
            "version": BUNDLE_VERSION,
        },
        "application": {
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": wikisyncd::PROTOCOL_VERSION,
        },
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "storage": storage,
        "catalog": catalog,
        "recent_runs": recent_runs,
        "control_plane": control_plane,
        "quick_logical_object_verification": verification,
        "source_reachability": source_reachability,
    })
}

fn source_reachability_section(library: &Library, online: bool) -> Value {
    if !online {
        return reachability_not_requested();
    }

    let sources = match library.wikis() {
        Ok(sources) => sources,
        Err(_) => return section_error("query-failed"),
    };
    let source_count = sources.len();
    let mut reachable = 0_usize;
    let mut unreachable = 0_usize;
    let mut configuration_rejected = 0_usize;
    let retry_policy = RetryPolicy::new(1, Duration::from_millis(1), Duration::from_millis(1))
        .expect("one-attempt reachability policy is valid");
    let probe_title = PageTitle::new("Main Page").expect("static probe title is valid");
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return section_error("runtime-unavailable"),
    };

    for source in sources.iter().take(REACHABILITY_SOURCE_LIMIT) {
        let Ok(user_agent) = application_user_agent() else {
            configuration_rejected += 1;
            continue;
        };
        let config = ClientConfig::new(&source.api_endpoint, user_agent).and_then(|config| {
            config
                .with_timeouts(REACHABILITY_REQUEST_TIMEOUT, REACHABILITY_CONNECT_TIMEOUT)?
                .with_max_response_bytes(REACHABILITY_RESPONSE_LIMIT)?
                .with_max_downloaded_response_bytes_per_run(REACHABILITY_RESPONSE_LIMIT)?
                .with_max_concurrent_requests(1)
                .map(|config| config.with_retry_policy(retry_policy))
        });
        let Ok(config) = config else {
            configuration_rejected += 1;
            continue;
        };
        let Ok(client) = MediaWikiClient::new(config) else {
            configuration_rejected += 1;
            continue;
        };
        match runtime.block_on(client.resolve_titles(std::slice::from_ref(&probe_title))) {
            Ok(_) => reachable += 1,
            Err(_) => unreachable += 1,
        }
    }

    let checked = source_count.min(REACHABILITY_SOURCE_LIMIT);
    section_ok(json!({
        "requested": true,
        "source_count": source_count,
        "checked_count": checked,
        "omitted_count": source_count.saturating_sub(checked),
        "reachable_count": reachable,
        "unreachable_count": unreachable,
        "configuration_rejected_count": configuration_rejected,
        "bounds": {
            "maximum_sources": REACHABILITY_SOURCE_LIMIT,
            "maximum_requests_per_source": 1,
            "request_timeout_seconds": REACHABILITY_REQUEST_TIMEOUT.as_secs(),
            "connect_timeout_seconds": REACHABILITY_CONNECT_TIMEOUT.as_secs(),
            "maximum_response_bytes_per_source": REACHABILITY_RESPONSE_LIMIT,
        },
    }))
}

fn reachability_not_requested() -> Value {
    section_ok(json!({ "requested": false }))
}

fn storage_section(library_root: &Path) -> Value {
    let current_uid = current_effective_uid();
    let database = file_summary(&library_root.join("library.sqlite3"), current_uid);
    let wal = file_summary(&library_root.join("library.sqlite3-wal"), current_uid);
    let shm = file_summary(&library_root.join("library.sqlite3-shm"), current_uid);
    let space = filesystem_space(library_root).map_or_else(
        || section_error("unavailable"),
        |(free_bytes, total_bytes)| {
            section_ok(json!({
                "free_bytes": free_bytes,
                "total_bytes": total_bytes,
            }))
        },
    );
    section_ok(json!({
        "database": database,
        "wal": wal,
        "shm": shm,
        "filesystem_space": space,
    }))
}

fn file_summary(path: &Path, current_uid: u32) -> Value {
    match fs::symlink_metadata(path) {
        Ok(metadata) => json!({
            "present": true,
            "regular_file": metadata.is_file(),
            "size_bytes": metadata.len(),
            "permissions_private": metadata.mode() & 0o077 == 0,
            "owner_current_user": metadata.uid() == current_uid,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({
            "present": false,
            "regular_file": false,
            "size_bytes": 0,
            "permissions_private": false,
            "owner_current_user": false,
        }),
        Err(_) => section_error("metadata-unavailable"),
    }
}

fn current_effective_uid() -> u32 {
    // `id` is used instead of unsafe platform FFI; failure produces a value that
    // cannot accidentally mark a file as acceptably owned.
    Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

fn filesystem_space(path: &Path) -> Option<(u64, u64)> {
    // Absolute system utility path avoids PATH-dependent execution. Parsing only
    // fixed numeric columns means mount and filesystem names are never retained.
    let output = Command::new("/bin/df").arg("-Pk").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(output.stdout).ok()?;
    let fields = output
        .lines()
        .last()?
        .split_whitespace()
        .collect::<Vec<_>>();
    let total_kib = fields.get(1)?.parse::<u64>().ok()?;
    let free_kib = fields.get(3)?.parse::<u64>().ok()?;
    Some((
        free_kib.saturating_mul(1_024),
        total_kib.saturating_mul(1_024),
    ))
}

fn catalog_section(library: &Library) -> Value {
    let result = (|| {
        let schema_version = library.schema_version()?;
        let sources = library.wikis()?.len();
        let collections = library.collections()?.len();
        let schedules = library.schedules()?;
        let mut manual = 0_u64;
        let mut interval = 0_u64;
        let mut daily_utc = 0_u64;
        let mut paused = 0_u64;
        for schedule in schedules {
            match schedule.cadence {
                ScheduleCadence::Manual => manual += 1,
                ScheduleCadence::Interval(_) => interval += 1,
                ScheduleCadence::DailyUtc(_) => daily_utc += 1,
            }
            paused += u64::from(schedule.paused);
        }
        Ok::<_, wikisync_store::StoreError>(json!({
            "snapshot": "read-only-checkpointed",
            "live_state_guaranteed": false,
            "schema_version": schema_version,
            "source_count": sources,
            "collection_count": collections,
            "schedule_counts": {
                "total": manual + interval + daily_utc,
                "manual": manual,
                "interval": interval,
                "daily_utc": daily_utc,
                "paused": paused,
            },
        }))
    })();
    result.map_or_else(|_| section_error("query-failed"), section_ok)
}

fn recent_runs_section(library: &Library) -> Value {
    let runs = match library.sync_run_statuses(RECENT_RUN_LIMIT) {
        Ok(runs) => runs,
        Err(_) => return section_error("query-failed"),
    };
    let mut running = 0_u64;
    let mut succeeded = 0_u64;
    let mut cancelled = 0_u64;
    let mut queued_jobs = 0_u64;
    let mut running_jobs = 0_u64;
    let mut succeeded_jobs = 0_u64;
    let mut failed_jobs = 0_u64;
    let mut recent_errors = Vec::new();
    for run in &runs {
        match run.state {
            SyncRunState::Running => running += 1,
            SyncRunState::Succeeded => succeeded += 1,
            SyncRunState::Cancelled => cancelled += 1,
        }
        queued_jobs = queued_jobs.saturating_add(run.queued_jobs);
        running_jobs = running_jobs.saturating_add(run.running_jobs);
        succeeded_jobs = succeeded_jobs.saturating_add(run.succeeded_jobs);
        failed_jobs = failed_jobs.saturating_add(run.failed_jobs);
        if let Some(error) = &run.latest_error {
            recent_errors.push(json!({
                "code": error.code,
                "retryable": error.retryable,
                "occurred_at": error.occurred_at,
            }));
        }
    }
    section_ok(json!({
        "snapshot": "read-only-checkpointed",
        "live_state_guaranteed": false,
        "limit": RECENT_RUN_LIMIT,
        "runs_observed": runs.len(),
        "state_counts": {
            "running": running,
            "succeeded": succeeded,
            "cancelled": cancelled,
        },
        "job_counts": {
            "queued": queued_jobs,
            "running": running_jobs,
            "succeeded": succeeded_jobs,
            "failed": failed_jobs,
        },
        "recent_errors": recent_errors,
    }))
}

fn verification_section(library: &Library) -> Value {
    match verify_library(library, VerificationScope::Quick) {
        Ok(report) => section_ok(json!({
            "scope": "quick",
            "snapshot": "read-only-checkpointed",
            "live_state_guaranteed": false,
            "coverage": match report.coverage {
                VerificationCoverage::Complete => "complete-logical-object-catalog",
                VerificationCoverage::Partial => "partial-logical-object-catalog",
            },
            "objects_at_start": report.objects_at_start,
            "objects_at_end": report.objects_at_end,
            "objects_examined": report.objects_examined,
            "objects_verified": report.objects_verified,
            "canonical_bytes_verified": report.canonical_bytes_verified,
            "finding_count": report.finding_count,
            "omitted_findings": report.finding_count,
        })),
        Err(_) => section_error("verification-failed"),
    }
}

fn control_plane_section(library_root: &Path) -> Value {
    let state = match inspect_control_plane(library_root) {
        Ok(state) => state,
        Err(_) => return section_error("inspection-failed"),
    };
    let daemon = socket_state(state.daemon);
    let writer = socket_state(state.writer);
    let health = if state.daemon == LocalSocketState::Active {
        match Client::for_library(library_root).and_then(|client| client.health()) {
            Ok(health) => section_ok(json!({
                "daemon_version": health.daemon_version,
                "uptime_seconds": health.uptime_seconds,
            })),
            Err(_) => section_error("health-unavailable"),
        }
    } else {
        section_ok(json!({ "available": false }))
    };
    section_ok(json!({
        "daemon_socket": daemon,
        "writer_socket": writer,
        "daemon_health": health,
    }))
}

fn socket_state(state: LocalSocketState) -> &'static str {
    match state {
        LocalSocketState::Missing => "missing",
        LocalSocketState::Active => "active",
        LocalSocketState::Stale => "stale",
        LocalSocketState::UnexpectedPath => "unexpected-path",
    }
}

fn section_ok(data: Value) -> Value {
    json!({ "status": "ok", "data": data })
}

fn section_error(code: &'static str) -> Value {
    json!({ "status": "error", "error": { "code": code } })
}

fn human_summary(report: &Value) -> String {
    let status = |section: &str| {
        report
            .pointer(&format!("/{section}/status"))
            .and_then(Value::as_str)
            .unwrap_or("unavailable")
    };
    let mut lines = vec![format!(
        "WikiSyncer doctor bundle format {BUNDLE_VERSION} (application {}, protocol {})",
        env!("CARGO_PKG_VERSION"),
        wikisyncd::PROTOCOL_VERSION,
    )];
    lines.push(format!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    lines.push(format!("Storage metadata: {}", status("storage")));
    if let Some(size) = report
        .pointer("/storage/data/database/size_bytes")
        .and_then(Value::as_u64)
    {
        let private = report
            .pointer("/storage/data/database/permissions_private")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owned = report
            .pointer("/storage/data/database/owner_current_user")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        lines.push(format!(
            "Database: {size} bytes; private permissions={private}; current owner={owned}"
        ));
    }
    lines.push(format!("Library catalog: {}", status("catalog")));
    if let (Some(schema), Some(sources), Some(collections)) = (
        report
            .pointer("/catalog/data/schema_version")
            .and_then(Value::as_u64),
        report
            .pointer("/catalog/data/source_count")
            .and_then(Value::as_u64),
        report
            .pointer("/catalog/data/collection_count")
            .and_then(Value::as_u64),
    ) {
        lines.push(format!(
            "Catalog aggregates: schema {schema}; {sources} sources; {collections} collections"
        ));
    }
    lines.push(
        "Library values use a read-only checkpointed snapshot and may lag an active writer."
            .to_owned(),
    );
    lines.push(format!(
        "Recent synchronization runs: {}",
        status("recent_runs")
    ));
    lines.push(format!("Local control plane: {}", status("control_plane")));
    if let (Some(daemon), Some(writer)) = (
        report
            .pointer("/control_plane/data/daemon_socket")
            .and_then(Value::as_str),
        report
            .pointer("/control_plane/data/writer_socket")
            .and_then(Value::as_str),
    ) {
        lines.push(format!("Control sockets: daemon={daemon}; writer={writer}"));
    }
    lines.push(format!(
        "Quick logical-object verification: {}",
        status("quick_logical_object_verification")
    ));
    if let (Some(coverage), Some(examined), Some(verified), Some(findings)) = (
        report
            .pointer("/quick_logical_object_verification/data/coverage")
            .and_then(Value::as_str),
        report
            .pointer("/quick_logical_object_verification/data/objects_examined")
            .and_then(Value::as_u64),
        report
            .pointer("/quick_logical_object_verification/data/objects_verified")
            .and_then(Value::as_u64),
        report
            .pointer("/quick_logical_object_verification/data/finding_count")
            .and_then(Value::as_u64),
    ) {
        lines.push(format!(
            "Verification aggregates: coverage={coverage}; examined={examined}; verified={verified}; findings={findings}"
        ));
    }
    lines.push(
        "Verification reports bounded logical-object coverage only; it is not a whole-archive trust claim."
            .to_owned(),
    );
    if report
        .pointer("/source_reachability/data/requested")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if let (Some(checked), Some(reachable), Some(unreachable), Some(rejected), Some(omitted)) = (
            report
                .pointer("/source_reachability/data/checked_count")
                .and_then(Value::as_u64),
            report
                .pointer("/source_reachability/data/reachable_count")
                .and_then(Value::as_u64),
            report
                .pointer("/source_reachability/data/unreachable_count")
                .and_then(Value::as_u64),
            report
                .pointer("/source_reachability/data/configuration_rejected_count")
                .and_then(Value::as_u64),
            report
                .pointer("/source_reachability/data/omitted_count")
                .and_then(Value::as_u64),
        ) {
            lines.push(format!(
                "Online source reachability: checked={checked}; reachable={reachable}; unreachable={unreachable}; configuration-rejected={rejected}; omitted={omitted}"
            ));
        } else {
            lines.push(format!(
                "Online source reachability: {}",
                status("source_reachability")
            ));
        }
    } else {
        lines.push("Online source reachability: not requested (offline default).".to_owned());
    }
    lines.join("\n")
}

fn write_bundle(path: &Path, report: &Value) -> Result<(), DoctorError> {
    let bytes = serde_json::to_vec_pretty(report)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                DoctorError::BundleExists
            } else {
                DoctorError::Io(error)
            }
        })?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Failure to render or create the requested diagnostic output.
#[derive(Debug)]
pub(crate) enum DoctorError {
    /// The bundle target already exists and must never be overwritten.
    BundleExists,
    /// Local output I/O failed.
    Io(io::Error),
    /// The allowlisted report could not be encoded.
    Json(serde_json::Error),
}

impl fmt::Display for DoctorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BundleExists => formatter.write_str("diagnostic bundle target already exists"),
            Self::Io(error) => write!(formatter, "diagnostic output failed: {error}"),
            Self::Json(error) => write!(formatter, "diagnostic JSON encoding failed: {error}"),
        }
    }
}

impl Error for DoctorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::BundleExists => None,
        }
    }
}

impl From<io::Error> for DoctorError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DoctorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
