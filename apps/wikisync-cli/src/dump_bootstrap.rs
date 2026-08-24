use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use serde_json::{Value, json};
use wikisync_core::{CollectionId, HistoryPolicy};
use wikisync_mediawiki::{DumpDigest, TrustedDumpIndex};
use wikisync_store::{CollectionStatus, Library, StoreError};
use wikisyncd::{
    CurrentDumpBootstrapOutcome, CurrentDumpBootstrapRequest, WriterAccess,
    bootstrap_collection_from_current_dump_direct, preview_current_dump_bootstrap,
};

const TRUST_WARNING: &str = "The BLAKE3 digest must be retained independently before this operation. A checksum downloaded beside or stored only with the index is not an independent trust anchor.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Command {
    pub(crate) collection_id: CollectionId,
    pub(crate) index_url: String,
    pub(crate) index_digest: DumpDigest,
    pub(crate) expected_database: String,
    pub(crate) commit: bool,
    pub(crate) json: bool,
}

pub(crate) fn parse(values: Vec<String>) -> Result<Command, String> {
    let mut collection_id = None;
    let mut index_url = None;
    let mut index_digest = None;
    let mut expected_database = None;
    let mut commit = false;
    let mut json = false;
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        let required = |values: &mut std::vec::IntoIter<String>, option: &str| {
            values
                .next()
                .ok_or_else(|| format!("{option} requires a value"))
        };
        match value.as_str() {
            "--collection" => {
                let raw = required(&mut values, "--collection")?
                    .parse::<u64>()
                    .map_err(|_| "--collection requires a positive integer".to_owned())?;
                let parsed = CollectionId::new(raw).map_err(|error| error.to_string())?;
                replace_once(&mut collection_id, parsed, "--collection")?;
            }
            "--index-url" => {
                let parsed = required(&mut values, "--index-url")?;
                replace_once(&mut index_url, parsed, "--index-url")?;
            }
            "--index-blake3" => {
                let raw = required(&mut values, "--index-blake3")?;
                let parsed = DumpDigest::from_hex(&raw).map_err(|error| {
                    format!("--index-blake3 requires exactly 64 hexadecimal digits: {error}")
                })?;
                replace_once(&mut index_digest, parsed, "--index-blake3")?;
            }
            "--expected-database" => {
                let parsed = required(&mut values, "--expected-database")?;
                replace_once(&mut expected_database, parsed, "--expected-database")?;
            }
            "--commit" => commit = true,
            "--json" => json = true,
            _ => return Err(format!("unknown dump-bootstrap option {value:?}")),
        }
    }
    let collection_id =
        collection_id.ok_or_else(|| "dump-bootstrap requires --collection <id>".to_owned())?;
    let index_url =
        index_url.ok_or_else(|| "dump-bootstrap requires --index-url <url>".to_owned())?;
    let index_digest =
        index_digest.ok_or_else(|| "dump-bootstrap requires --index-blake3 <digest>".to_owned())?;
    let expected_database = expected_database
        .ok_or_else(|| "dump-bootstrap requires --expected-database <name>".to_owned())?;
    // Validate the complete caller-retained trust identity before any library is
    // opened or any network/mutation decision is made.
    TrustedDumpIndex::new(&index_url, index_digest, &expected_database)
        .map_err(|error| format!("invalid authenticated dump index identity: {error}"))?;
    Ok(Command {
        collection_id,
        index_url,
        index_digest,
        expected_database,
        commit,
        json,
    })
}

fn replace_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("{option} may only be supplied once"));
    }
    Ok(())
}

pub(crate) fn run(library_root: &Path, command: Command, schema_version: u32) -> Result<(), Error> {
    let trusted_index = TrustedDumpIndex::new(
        &command.index_url,
        command.index_digest,
        command.expected_database.clone(),
    )
    .map_err(|error| Error::Message(error.to_string()))?;
    let request = CurrentDumpBootstrapRequest::new(command.collection_id, trusted_index)
        .map_err(|error| Error::Message(error.to_string()))?;
    let library = Library::open_read_only(library_root)?;
    let preview = Preview::load(&library, &request, schema_version)?;
    drop(library);

    if !command.commit {
        return preview.write(command.json, false, None);
    }
    let committed_request = request
        .clone()
        .with_expected_collection_generation(preview.collection_generation);

    let outcome = match WriterAccess::discover(library_root)
        .map_err(|error| Error::Message(error.to_string()))?
    {
        WriterAccess::Daemon(client) => client
            .bootstrap_collection_from_current_dump(&committed_request)
            .map_err(|error| Error::Message(error.to_string()))?,
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(library_root)?;
            bootstrap_collection_from_current_dump_direct(&mut library, &committed_request)
                .map_err(|error| Error::Message(error.to_string()))?
        }
    };
    preview.write(command.json, true, Some(outcome_json(&outcome)))
}

#[derive(Debug)]
struct Preview {
    collection_generation: u64,
    json: Value,
    source_line: String,
    trust_line: String,
    scope_line: String,
    transfer_line: String,
    parser_line: String,
    budget_line: String,
}

impl Preview {
    fn load(
        library: &Library,
        request: &CurrentDumpBootstrapRequest,
        schema_version: u32,
    ) -> Result<Self, Error> {
        let service_preview = preview_current_dump_bootstrap(library, request)
            .map_err(|error| Error::Message(error.to_string()))?;
        let configuration = library
            .collection_configuration(service_preview.collection_id)?
            .ok_or_else(|| {
                Error::Message(format!(
                    "collection {} is not configured",
                    service_preview.collection_id
                ))
            })?;
        if configuration.status != CollectionStatus::Active {
            return Err(Error::Message(format!(
                "collection {} is tombstoned and cannot be bootstrapped",
                service_preview.collection_id
            )));
        }
        if configuration.history_policy != HistoryPolicy::CurrentAndFuture {
            return Err(Error::Message(format!(
                "collection {} uses {:?} history; current dump bootstrap requires current-and-future history",
                service_preview.collection_id, configuration.history_policy
            )));
        }
        let acquisition = service_preview.acquisition_limits;
        let parser = service_preview.parser_limits;
        let max_pages = service_preview.maximum_collection_pages;
        let max_bytes = service_preview.maximum_collection_canonical_bytes;

        let json = json!({
            "schema_version": schema_version,
            "operation": "dump-bootstrap",
            "committed": false,
            "source": {
                "wiki_id": service_preview.wiki_id.get(),
                "api_endpoint": service_preview.source_api_endpoint,
                "language_code": service_preview.source_language_code,
                "expected_database": service_preview.expected_database,
            },
            "trust": {
                "index_url": service_preview.index_url,
                "digest_algorithm": DumpDigest::ALGORITHM,
                "index_blake3": service_preview.index_digest,
                "caller_retained_independently": true,
                "warning": TRUST_WARNING,
            },
            "collection_scope": {
                "collection_id": service_preview.collection_id.get(),
                "name": configuration.name,
                "generation": service_preview.collection_generation,
                "status": configuration.status.as_str(),
                "history_policy": "current-and-future",
                "resolved_page_count": service_preview.selected_pages,
                "selection_identity": "durable-stable-page-ids",
            },
            "ceilings": {
                "transfer": {
                    "max_concurrent_requests": service_preview.max_concurrent_requests,
                    "max_download_bytes_per_second": service_preview.max_download_bytes_per_second,
                    "avoid_metered_networks": service_preview.avoid_metered_networks,
                    "max_index_bytes": acquisition.max_index_bytes,
                    "max_artifact_bytes": acquisition.max_artifact_bytes,
                    "max_total_artifact_bytes": acquisition.max_total_artifact_bytes,
                    "max_artifacts": acquisition.max_artifacts,
                    "max_elapsed_seconds": acquisition.max_elapsed.as_secs(),
                },
                "parser": {
                    "max_compressed_bytes": parser.max_compressed_bytes,
                    "max_decompressed_bytes": parser.max_decompressed_bytes,
                    "max_pages": parser.max_pages,
                    "max_page_xml_bytes": parser.max_page_xml_bytes,
                    "max_text_bytes": parser.max_text_bytes,
                    "max_metadata_field_bytes": parser.max_metadata_field_bytes,
                    "max_siteinfo_bytes": parser.max_siteinfo_bytes,
                    "max_namespaces": parser.max_namespaces,
                },
                "storage": {
                    "cache_directory": service_preview.cache_directory,
                    "cache_directory_is_library_relative": true,
                    "max_cached_compressed_bytes": acquisition.max_total_artifact_bytes,
                    "hard_collection_maximum_pages": max_pages,
                    "hard_collection_maximum_canonical_bytes": max_bytes,
                },
            },
        });

        Ok(Self {
            collection_generation: service_preview.collection_generation,
            source_line: format!(
                "Source: wiki {} ({}), API {}, expected dump database {}.",
                service_preview.wiki_id,
                service_preview.source_language_code,
                service_preview.source_api_endpoint,
                service_preview.expected_database
            ),
            trust_line: format!(
                "Trusted index: {} ({} {}).",
                service_preview.index_url,
                DumpDigest::ALGORITHM,
                service_preview.index_digest
            ),
            scope_line: format!(
                "Collection scope: {} ({:?}), generation {}, {} durable stable page IDs; current-and-future history.",
                service_preview.collection_id,
                configuration.name,
                service_preview.collection_generation,
                service_preview.selected_pages
            ),
            transfer_line: format!(
                "Transfer/storage ceilings: {} concurrent requests, rate {}, metered-network avoidance {}, {} artifacts / {} total compressed bytes, {} index bytes, {} seconds; cache {}.",
                service_preview.max_concurrent_requests,
                optional_limit(service_preview.max_download_bytes_per_second),
                service_preview.avoid_metered_networks,
                acquisition.max_artifacts,
                acquisition.max_total_artifact_bytes,
                acquisition.max_index_bytes,
                acquisition.max_elapsed.as_secs(),
                service_preview.cache_directory,
            ),
            parser_line: format!(
                "Parser ceilings: {} compressed bytes, {} decompressed bytes, {} pages, {} page XML bytes, {} revision-text bytes.",
                parser.max_compressed_bytes,
                parser.max_decompressed_bytes,
                parser.max_pages,
                parser.max_page_xml_bytes,
                parser.max_text_bytes,
            ),
            budget_line: format!(
                "Hard collection budgets: pages {}, canonical bytes {}.",
                optional_limit(max_pages),
                optional_limit(max_bytes)
            ),
            json,
        })
    }

    fn write(
        mut self,
        json_output: bool,
        committed: bool,
        result: Option<Value>,
    ) -> Result<(), Error> {
        self.json["committed"] = Value::Bool(committed);
        if let Some(result) = result {
            self.json["result"] = result;
        }
        if json_output {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer_pretty(&mut output, &self.json)?;
            output.write_all(b"\n")?;
        } else if committed {
            println!("Authenticated current-dump bootstrap completed.");
            if let Some(result) = self.json.get("result") {
                println!(
                    "Run {}, import {}: {} pages scanned; {} imported, {} reused, {} absent from the dump.",
                    result["run_id"],
                    result["import_id"],
                    result["progress"]["pages_scanned"],
                    result["progress"]["pages_imported"],
                    result["progress"]["pages_reused"],
                    result["progress"]["pages_absent_from_dump"],
                );
                println!(
                    "Race closure checked {} pages; checkpoint committed through {}.",
                    result["closure"]["pages_checked"], result["checkpoint_committed_through"],
                );
            }
            println!("Warning: {TRUST_WARNING}");
        } else {
            println!("Dump bootstrap preview (no network transfer or mutation performed).\n");
            println!("{}", self.source_line);
            println!("{}", self.trust_line);
            println!("{}", self.scope_line);
            println!("{}", self.transfer_line);
            println!("{}", self.parser_line);
            println!("{}", self.budget_line);
            println!("Warning: {TRUST_WARNING}");
            println!("Re-run with --commit to acquire, authenticate, and import the dump set.");
        }
        Ok(())
    }
}

fn outcome_json(outcome: &CurrentDumpBootstrapOutcome) -> Value {
    json!({
        "kind": "completed",
        "run_id": outcome.run_id,
        "import_id": outcome.import_id,
        "import_state": outcome.import_state.as_str(),
        "resumed": outcome.resumed,
        "progress": {
            "pages_scanned": outcome.pages_scanned,
            "pages_imported": outcome.pages_imported,
            "pages_reused": outcome.pages_reused,
            "pages_absent_from_dump": outcome.pages_absent_from_dump,
        },
        "closure": {
            "pages_checked": outcome.closure_pages_checked,
            "differing_heads": outcome.closure_differing_heads,
            "missing_pages": outcome.closure_missing_pages,
            "pages_captured_from_api": outcome.closure_pages_captured_from_api,
        },
        "checkpoint_committed_through": outcome.checkpoint_committed_through,
    })
}

fn optional_limit(value: Option<u64>) -> String {
    value.map_or_else(|| "unlimited".to_owned(), |value| value.to_string())
}

#[derive(Debug)]
pub(crate) enum Error {
    Message(String),
    Store(StoreError),
    Json(serde_json::Error),
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => message.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl StdError for Error {}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
