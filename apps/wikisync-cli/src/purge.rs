use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;

use serde_json::json;
use wikisync_core::CollectionId;
use wikisync_store::{Library, PurgeJournalState, StoreError};
use wikisyncd::{
    ApplicationHandler, CollectionPurgeRequest, DaemonError, OperationControl, OperationError,
    RequestHandler, WriterAccess, collection_purge_mutation, decode_collection_purge_outcome,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Preview {
        collection_id: CollectionId,
        json: bool,
    },
    Execute {
        collection_id: CollectionId,
        collection_name: String,
        preview_fingerprint: String,
        payload_only_acknowledged: bool,
        external_copies_not_erased_acknowledged: bool,
        json: bool,
    },
}

pub(crate) fn parse(values: Vec<String>) -> Result<Command, String> {
    let mut values = values.into_iter();
    let operation = values
        .next()
        .ok_or_else(|| "purge requires preview or execute".to_owned())?;
    let mut collection_id = None;
    let mut collection_name = None;
    let mut preview_fingerprint = None;
    let mut payload_only_acknowledged = false;
    let mut external_copies_not_erased_acknowledged = false;
    let mut json = false;
    while let Some(value) = values.next() {
        match value.as_str() {
            "--collection" => {
                collection_id = Some(parse_collection_id(
                    &values
                        .next()
                        .ok_or_else(|| "--collection requires an ID".to_owned())?,
                )?);
            }
            "--name" if operation == "execute" => {
                collection_name = Some(
                    values
                        .next()
                        .ok_or_else(|| "--name requires the exact previewed name".to_owned())?,
                );
            }
            "--fingerprint" if operation == "execute" => {
                preview_fingerprint = Some(values.next().ok_or_else(|| {
                    "--fingerprint requires the exact preview fingerprint".to_owned()
                })?);
            }
            "--ack-payload-only-and-retain-audit" if operation == "execute" => {
                payload_only_acknowledged = true;
            }
            "--ack-external-copies-not-erased" if operation == "execute" => {
                external_copies_not_erased_acknowledged = true;
            }
            "--json" => json = true,
            _ => return Err(format!("unknown purge {operation} option {value:?}")),
        }
    }
    let collection_id = collection_id.ok_or_else(|| "--collection is required".to_owned())?;
    match operation.as_str() {
        "preview" => Ok(Command::Preview {
            collection_id,
            json,
        }),
        "execute" => Ok(Command::Execute {
            collection_id,
            collection_name: collection_name.ok_or_else(|| "--name is required".to_owned())?,
            preview_fingerprint: preview_fingerprint
                .ok_or_else(|| "--fingerprint is required".to_owned())?,
            payload_only_acknowledged,
            external_copies_not_erased_acknowledged,
            json,
        }),
        _ => Err(format!("unknown purge operation {operation:?}")),
    }
}

pub(crate) fn run(library_root: &Path, command: Command, schema_version: u32) -> Result<(), Error> {
    match command {
        Command::Preview {
            collection_id,
            json,
        } => {
            let library = Library::open_read_only(library_root)?;
            let preview = library.preview_collection_purge(collection_id)?;
            if json {
                write_json(&json!({
                    "schema_version": schema_version,
                    "collection_id": preview.collection_id.get(),
                    "collection_name": preview.collection_name,
                    "collection_generation": preview.collection_generation,
                    "tombstoned_at": preview.tombstoned_at,
                    "manifest_head_sequence": preview.manifest_head_sequence,
                    "manifest_head_id": preview.manifest_head_id.map(|id| id.to_string()),
                    "catalog_fingerprint": preview.catalog_fingerprint,
                    "preview_fingerprint": preview.fingerprint,
                    "object_count": preview.object_count,
                    "wikitext_object_count": preview.wikitext_object_count,
                    "media_object_count": preview.media_object_count,
                    "logical_bytes": preview.logical_bytes,
                    "reclaimable_bytes": preview.reclaimable_bytes,
                    "loose_object_count": preview.loose_object_count,
                    "affected_pack_count": preview.affected_pack_count,
                    "whole_pack_count": preview.whole_pack_count,
                    "mixed_pack_count": preview.mixed_pack_count,
                    "payload_only_audit_retained": true,
                    "external_copies_erased": false,
                }))?;
            } else {
                println!("Collection purge preview (no mutation performed)");
                println!(
                    "Collection: {} ({})",
                    preview.collection_name, preview.collection_id
                );
                println!("Preview fingerprint: {}", preview.fingerprint);
                println!(
                    "Payload: {} exclusive object(s), {} logical bytes; {} currently reclaimable bytes",
                    preview.object_count, preview.logical_bytes, preview.reclaimable_bytes
                );
                println!(
                    "Storage: {} loose object(s), {} whole pack(s), {} mixed pack(s)",
                    preview.loose_object_count, preview.whole_pack_count, preview.mixed_pack_count
                );
                println!("Audit metadata, identities, hashes, and purge evidence are retained.");
                println!(
                    "Backups, snapshots, exports, external copies, and storage-device remnants are not erased."
                );
            }
            Ok(())
        }
        Command::Execute {
            collection_id,
            collection_name,
            preview_fingerprint,
            payload_only_acknowledged,
            external_copies_not_erased_acknowledged,
            json,
        } => {
            let request = CollectionPurgeRequest {
                collection_id,
                collection_name,
                preview_fingerprint,
                payload_only_acknowledged,
                external_copies_not_erased_acknowledged,
            };
            let mutation = collection_purge_mutation(&request)?;
            let raw = match WriterAccess::discover(library_root)? {
                WriterAccess::Daemon(client) => client.forward_mutation(mutation)?,
                WriterAccess::Direct(_lease) => {
                    let mut handler = ApplicationHandler::new(library_root)?;
                    handler.mutate(mutation, OperationControl::running())?
                }
            };
            let outcome = decode_collection_purge_outcome(&raw)?;
            if outcome.progress.state != PurgeJournalState::Succeeded {
                return Err(Error::Data(
                    "purge mutation returned before durable cleanup succeeded",
                ));
            }
            if json {
                write_json(&json!({
                    "schema_version": schema_version,
                    "purge_id": outcome.purge_id,
                    "state": state_name(outcome.progress.state),
                    "manifest_installed": outcome.progress.manifest_installed,
                    "retired_pack_count": outcome.progress.retired_pack_count,
                    "retired_file_count": outcome.progress.retired_file_count,
                    "retired_file_bytes": outcome.progress.retired_file_bytes,
                    "replacement_file_bytes": outcome.progress.replacement_file_bytes,
                    "net_reclaimed_file_bytes": outcome.progress.net_reclaimed_file_bytes,
                    "audit_metadata_retained": true,
                    "external_copies_erased": false,
                }))?;
            } else {
                println!("Collection purge {} completed.", outcome.purge_id);
                println!(
                    "Retired {} managed file(s) and {} bytes; replacement packs use {} bytes (net reclaimed: {} bytes).",
                    outcome.progress.retired_file_count,
                    outcome.progress.retired_file_bytes,
                    outcome.progress.replacement_file_bytes,
                    outcome.progress.net_reclaimed_file_bytes
                );
                println!(
                    "Audit metadata and hashes were retained; external copies were not erased."
                );
            }
            Ok(())
        }
    }
}

fn parse_collection_id(value: &str) -> Result<CollectionId, String> {
    value
        .parse::<u64>()
        .map_err(|_| "collection ID must be an unsigned integer".to_owned())
        .and_then(|value| CollectionId::new(value).map_err(|error| error.to_string()))
}

fn state_name(state: PurgeJournalState) -> &'static str {
    match state {
        PurgeJournalState::Authorized => "authorized",
        PurgeJournalState::Repacking => "repacking",
        PurgeJournalState::Cleaning => "cleaning",
        PurgeJournalState::Succeeded => "succeeded",
        PurgeJournalState::Failed => "failed",
    }
}

fn write_json(value: &serde_json::Value) -> Result<(), Error> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum Error {
    Store(StoreError),
    Daemon(DaemonError),
    Operation(OperationError),
    Json(serde_json::Error),
    Io(io::Error),
    Data(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::Daemon(error) => error.fmt(formatter),
            Self::Operation(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Data(message) => formatter.write_str(message),
        }
    }
}

impl StdError for Error {}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DaemonError> for Error {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<OperationError> for Error {
    fn from(error: OperationError) -> Self {
        Self::Operation(error)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_parser_keeps_acknowledgements_independent() {
        let command = parse(vec![
            "execute".to_owned(),
            "--collection".to_owned(),
            "9".to_owned(),
            "--name".to_owned(),
            "Exact".to_owned(),
            "--fingerprint".to_owned(),
            "abc".to_owned(),
            "--ack-payload-only-and-retain-audit".to_owned(),
        ])
        .expect("parse");
        assert!(matches!(
            command,
            Command::Execute {
                payload_only_acknowledged: true,
                external_copies_not_erased_acknowledged: false,
                ..
            }
        ));
    }
}
