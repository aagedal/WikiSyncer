//! Bounded product-facing collection-purge mutation contract.

use wikisync_core::CollectionId;
use wikisync_store::{
    Library, PurgeAuthorization, PurgeCleanupProgress, PurgeCleanupStep, PurgeJournalState,
};

use crate::{Mutation, MutationOutcome, OperationControl, OperationError};

/// Versioned extension name for an exact, acknowledged collection purge.
pub const COLLECTION_PURGE_EXTENSION: &str = "collection-purge-v1";
/// Stable successful result name returned by the purge extension.
pub const COLLECTION_PURGE_RESULT: &str = "collection-purge-complete-v1";
const CONTRACT_VERSION: u8 = 1;
const MAX_COLLECTION_NAME_BYTES: usize = 4 * 1024;
const MAX_FINGERPRINT_BYTES: usize = 128;
const RECOVERY_PAGE_SIZE: u32 = 1_000;

/// Exact operator confirmations encoded into one purge mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionPurgeRequest {
    pub collection_id: CollectionId,
    pub collection_name: String,
    pub preview_fingerprint: String,
    pub payload_only_acknowledged: bool,
    pub external_copies_not_erased_acknowledged: bool,
}

/// Terminal durable receipt returned by the purge mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionPurgeOutcome {
    pub purge_id: u64,
    pub progress: PurgeCleanupProgress,
}

/// Builds the bounded daemon mutation used by direct and IPC writers.
pub fn collection_purge_mutation(
    request: &CollectionPurgeRequest,
) -> Result<Mutation, OperationError> {
    Ok(Mutation::Extension {
        name: COLLECTION_PURGE_EXTENSION.to_owned(),
        payload: encode_request(request)?,
    })
}

/// Decodes and validates the stable purge mutation result.
pub fn decode_collection_purge_outcome(
    outcome: &MutationOutcome,
) -> Result<CollectionPurgeOutcome, OperationError> {
    if outcome.result != COLLECTION_PURGE_RESULT {
        return Err(OperationError::failed(
            "unexpected collection purge result name",
        ));
    }
    decode_outcome(&outcome.payload)
}

pub(crate) fn execute_collection_purge(
    library: &mut Library,
    payload: &[u8],
    control: &OperationControl,
) -> Result<MutationOutcome, OperationError> {
    let request = decode_request(payload)?;
    require_acknowledgements(&request)?;
    if control.is_shutdown_requested() {
        return Err(OperationError::failed(
            "collection purge cancelled before authorization",
        ));
    }
    let (_, recovered_match) =
        recover_unfinished_purges_matching(library, control, Some(&request))?;
    if let Some(progress) = recovered_match {
        let response = CollectionPurgeOutcome {
            purge_id: progress.purge_id,
            progress,
        };
        return Ok(MutationOutcome {
            result: COLLECTION_PURGE_RESULT.to_owned(),
            payload: encode_outcome(&response),
        });
    }
    let receipt = library
        .authorize_collection_purge(
            request.collection_id,
            PurgeAuthorization {
                collection_name: &request.collection_name,
                preview_fingerprint: &request.preview_fingerprint,
                payload_only_acknowledged: request.payload_only_acknowledged,
                backups_not_erased_acknowledged: request.external_copies_not_erased_acknowledged,
            },
        )
        .map_err(failed)?;
    let progress = drain_purge(library, receipt.purge_id, control)?;
    let response = CollectionPurgeOutcome {
        purge_id: receipt.purge_id,
        progress,
    };
    Ok(MutationOutcome {
        result: COLLECTION_PURGE_RESULT.to_owned(),
        payload: encode_outcome(&response),
    })
}

/// Resumes every durable unfinished purge in oldest-first bounded pages.
pub(crate) fn recover_unfinished_purges(
    library: &mut Library,
    control: &OperationControl,
) -> Result<u64, OperationError> {
    recover_unfinished_purges_matching(library, control, None).map(|(count, _)| count)
}

fn recover_unfinished_purges_matching(
    library: &mut Library,
    control: &OperationControl,
    request: Option<&CollectionPurgeRequest>,
) -> Result<(u64, Option<PurgeCleanupProgress>), OperationError> {
    let mut completed = 0_u64;
    let mut matching_purge_id = None;
    loop {
        let unfinished = library
            .unfinished_purge_cleanups(None, RECOVERY_PAGE_SIZE)
            .map_err(failed)?;
        if unfinished.is_empty() {
            let matching_progress = matching_purge_id
                .map(|purge_id| library.purge_cleanup_progress(purge_id).map_err(failed))
                .transpose()?;
            return Ok((completed, matching_progress));
        }
        for progress in unfinished {
            if let Some(request) = request {
                let event = library
                    .purge_verification_snapshot(progress.purge_id)
                    .map_err(failed)?
                    .expected_manifest;
                if event.collection_id == request.collection_id
                    && event.collection_name == request.collection_name
                    && event.preview_fingerprint == request.preview_fingerprint
                {
                    matching_purge_id = Some(progress.purge_id);
                }
            }
            drain_purge(library, progress.purge_id, control)?;
            completed = completed.saturating_add(1);
        }
    }
}

fn require_acknowledgements(request: &CollectionPurgeRequest) -> Result<(), OperationError> {
    if request.payload_only_acknowledged && request.external_copies_not_erased_acknowledged {
        Ok(())
    } else {
        Err(OperationError::failed(
            "collection purge requires separate payload-only/audit-retention and external-copy acknowledgements",
        ))
    }
}

fn drain_purge(
    library: &mut Library,
    purge_id: u64,
    control: &OperationControl,
) -> Result<PurgeCleanupProgress, OperationError> {
    loop {
        if control.is_shutdown_requested() {
            return Err(OperationError::failed(format!(
                "collection purge {purge_id} paused safely for shutdown"
            )));
        }
        let advance = library.resume_purge_cleanup(purge_id).map_err(failed)?;
        match advance.step {
            PurgeCleanupStep::Completed | PurgeCleanupStep::AlreadyComplete => {
                return Ok(advance.progress);
            }
            PurgeCleanupStep::ReplacementRequired => {
                return Err(OperationError::failed(format!(
                    "collection purge {purge_id} requires mixed-pack replacement support; startup and conflicting mutations fail closed"
                )));
            }
            PurgeCleanupStep::Prepared
            | PurgeCleanupStep::WholePackReady
            | PurgeCleanupStep::ReplacementReady
            | PurgeCleanupStep::AuthorizedAbsenceCommitted
            | PurgeCleanupStep::FilesRetired => {}
        }
    }
}

fn encode_request(request: &CollectionPurgeRequest) -> Result<Vec<u8>, OperationError> {
    validate_text(
        &request.collection_name,
        MAX_COLLECTION_NAME_BYTES,
        "collection name",
    )?;
    validate_text(
        &request.preview_fingerprint,
        MAX_FINGERPRINT_BYTES,
        "preview fingerprint",
    )?;
    let mut bytes = vec![CONTRACT_VERSION];
    bytes.extend_from_slice(&request.collection_id.get().to_be_bytes());
    put_text(&mut bytes, &request.collection_name);
    put_text(&mut bytes, &request.preview_fingerprint);
    bytes.push(u8::from(request.payload_only_acknowledged));
    bytes.push(u8::from(request.external_copies_not_erased_acknowledged));
    Ok(bytes)
}

fn decode_request(payload: &[u8]) -> Result<CollectionPurgeRequest, OperationError> {
    let mut decoder = Decoder::new(payload);
    if decoder.u8()? != CONTRACT_VERSION {
        return Err(OperationError::failed(
            "unsupported collection purge payload version",
        ));
    }
    let collection_id = CollectionId::new(decoder.u64()?)
        .map_err(|error| OperationError::failed(error.to_string()))?;
    let collection_name = decoder.text(MAX_COLLECTION_NAME_BYTES)?;
    let preview_fingerprint = decoder.text(MAX_FINGERPRINT_BYTES)?;
    let payload_only_acknowledged = decoder.flag()?;
    let external_copies_not_erased_acknowledged = decoder.flag()?;
    decoder.finish()?;
    Ok(CollectionPurgeRequest {
        collection_id,
        collection_name,
        preview_fingerprint,
        payload_only_acknowledged,
        external_copies_not_erased_acknowledged,
    })
}

fn encode_outcome(outcome: &CollectionPurgeOutcome) -> Vec<u8> {
    let progress = &outcome.progress;
    let mut bytes = vec![CONTRACT_VERSION];
    for value in [
        outcome.purge_id,
        progress.purge_id,
        progress.pending_pack_count,
        progress.replacement_ready_pack_count,
        progress.retired_pack_count,
        progress.pending_file_count,
        progress.unlinking_file_count,
        progress.retired_file_count,
        progress.retired_file_bytes,
        progress.replacement_file_bytes,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&progress.net_reclaimed_file_bytes.to_be_bytes());
    bytes.push(state_tag(progress.state));
    bytes.push(u8::from(progress.manifest_installed));
    bytes
}

fn decode_outcome(payload: &[u8]) -> Result<CollectionPurgeOutcome, OperationError> {
    let mut decoder = Decoder::new(payload);
    if decoder.u8()? != CONTRACT_VERSION {
        return Err(OperationError::failed(
            "unsupported collection purge outcome version",
        ));
    }
    let purge_id = decoder.u64()?;
    let progress_purge_id = decoder.u64()?;
    let pending_pack_count = decoder.u64()?;
    let replacement_ready_pack_count = decoder.u64()?;
    let retired_pack_count = decoder.u64()?;
    let pending_file_count = decoder.u64()?;
    let unlinking_file_count = decoder.u64()?;
    let retired_file_count = decoder.u64()?;
    let retired_file_bytes = decoder.u64()?;
    let replacement_file_bytes = decoder.u64()?;
    let net_reclaimed_file_bytes = decoder.i64()?;
    let state = decode_state(decoder.u8()?)?;
    let manifest_installed = decoder.flag()?;
    decoder.finish()?;
    if purge_id != progress_purge_id {
        return Err(OperationError::failed(
            "collection purge outcome IDs did not match",
        ));
    }
    Ok(CollectionPurgeOutcome {
        purge_id,
        progress: PurgeCleanupProgress {
            purge_id,
            state,
            manifest_installed,
            pending_pack_count,
            replacement_ready_pack_count,
            retired_pack_count,
            pending_file_count,
            unlinking_file_count,
            retired_file_count,
            retired_file_bytes,
            replacement_file_bytes,
            net_reclaimed_file_bytes,
        },
    })
}

fn validate_text(value: &str, maximum: usize, label: &str) -> Result<(), OperationError> {
    if value.is_empty() || value.len() > maximum {
        return Err(OperationError::failed(format!(
            "collection purge {label} is outside its bound"
        )));
    }
    Ok(())
}

fn put_text(bytes: &mut Vec<u8>, value: &str) {
    let length = u32::try_from(value.len()).expect("validated purge text length fits u32");
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn state_tag(state: PurgeJournalState) -> u8 {
    match state {
        PurgeJournalState::Authorized => 1,
        PurgeJournalState::Repacking => 2,
        PurgeJournalState::Cleaning => 3,
        PurgeJournalState::Succeeded => 4,
        PurgeJournalState::Failed => 5,
    }
}

fn decode_state(tag: u8) -> Result<PurgeJournalState, OperationError> {
    match tag {
        1 => Ok(PurgeJournalState::Authorized),
        2 => Ok(PurgeJournalState::Repacking),
        3 => Ok(PurgeJournalState::Cleaning),
        4 => Ok(PurgeJournalState::Succeeded),
        5 => Ok(PurgeJournalState::Failed),
        _ => Err(OperationError::failed(
            "invalid collection purge journal state",
        )),
    }
}

fn failed(error: impl std::fmt::Display) -> OperationError {
    OperationError::failed(error.to_string())
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], OperationError> {
        let (value, remaining) = self.remaining.split_at_checked(count).ok_or_else(|| {
            OperationError::failed("truncated collection purge extension payload")
        })?;
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, OperationError> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, OperationError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn i64(&mut self) -> Result<i64, OperationError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn text(&mut self, maximum: usize) -> Result<String, OperationError> {
        let length = u32::from_be_bytes(self.take(4)?.try_into().expect("exact length")) as usize;
        if length == 0 || length > maximum {
            return Err(OperationError::failed(
                "collection purge text is outside its bound",
            ));
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| OperationError::failed("collection purge text is not valid UTF-8"))
    }

    fn flag(&mut self) -> Result<bool, OperationError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(OperationError::failed(
                "invalid collection purge acknowledgement flag",
            )),
        }
    }

    fn finish(self) -> Result<(), OperationError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(OperationError::failed(
                "trailing collection purge extension bytes",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use wikisync_core::{PageId, PageTitle, RevisionId};
    use wikisync_store::{CurrentRevisionCapture, PurgePreview};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-purge-{}-{unique}", std::process::id()));
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

    fn purge_fixture() -> (TempLibrary, Library, PurgePreview) {
        let temporary = TempLibrary::new();
        let mut library = Library::open(temporary.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Purge product fixture")
            .expect("create collection");
        let title = PageTitle::new("Exclusive purge page").expect("title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(1).expect("page ID"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(1).expect("revision ID"),
                    parent_id: None,
                    timestamp: "2026-08-25T10:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"exclusive purge product payload",
                },
            )
            .expect("capture revision");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("preview purge");
        (temporary, library, preview)
    }

    #[test]
    fn request_round_trip_preserves_separate_acknowledgements() {
        let request = CollectionPurgeRequest {
            collection_id: CollectionId::new(7).expect("collection ID"),
            collection_name: "Exact collection name".to_owned(),
            preview_fingerprint: "a".repeat(64),
            payload_only_acknowledged: true,
            external_copies_not_erased_acknowledged: false,
        };
        let encoded = encode_request(&request).expect("encode");
        assert_eq!(decode_request(&encoded).expect("decode"), request);
    }

    #[test]
    fn request_decoder_rejects_trailing_or_invalid_flag_bytes() {
        let request = CollectionPurgeRequest {
            collection_id: CollectionId::new(1).expect("collection ID"),
            collection_name: "Name".to_owned(),
            preview_fingerprint: "b".repeat(64),
            payload_only_acknowledged: true,
            external_copies_not_erased_acknowledged: true,
        };
        let mut encoded = encode_request(&request).expect("encode");
        encoded.push(0);
        assert!(decode_request(&encoded).is_err());
        encoded.pop();
        let last = encoded.len() - 1;
        encoded[last] = 2;
        assert!(decode_request(&encoded).is_err());
    }

    #[test]
    fn exact_acknowledged_mutation_reaches_terminal_cleanup() {
        let (_temporary, mut library, preview) = purge_fixture();
        let request = CollectionPurgeRequest {
            collection_id: preview.collection_id,
            collection_name: preview.collection_name,
            preview_fingerprint: preview.fingerprint,
            payload_only_acknowledged: true,
            external_copies_not_erased_acknowledged: true,
        };
        let raw = execute_collection_purge(
            &mut library,
            &encode_request(&request).expect("encode request"),
            &OperationControl::running(),
        )
        .expect("execute purge");
        let outcome = decode_collection_purge_outcome(&raw).expect("decode outcome");
        assert_eq!(outcome.progress.state, PurgeJournalState::Succeeded);
        assert!(outcome.progress.manifest_installed);
        assert!(
            library
                .unfinished_purge_cleanups(None, 1)
                .expect("unfinished purges")
                .is_empty()
        );
    }

    #[test]
    fn recovery_completes_an_authorized_journal_before_new_work() {
        let (_temporary, mut library, preview) = purge_fixture();
        let receipt = library
            .authorize_collection_purge(
                preview.collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        assert_eq!(
            recover_unfinished_purges(&mut library, &OperationControl::running())
                .expect("recover purge"),
            1
        );
        assert_eq!(
            library
                .purge_cleanup_progress(receipt.purge_id)
                .expect("purge progress")
                .state,
            PurgeJournalState::Succeeded
        );
    }

    #[test]
    fn daemon_bind_recovers_authorized_purge_before_exposing_its_socket() {
        let (temporary, mut library, preview) = purge_fixture();
        let receipt = library
            .authorize_collection_purge(
                preview.collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge before daemon restart");
        drop(library);

        let handler = crate::ApplicationHandler::new(temporary.path()).expect("daemon handler");
        let daemon = crate::Daemon::bind(temporary.path(), handler)
            .expect("bind daemon after startup recovery");
        let library = Library::open_read_only(temporary.path()).expect("inspect recovered purge");
        assert_eq!(
            library
                .purge_cleanup_progress(receipt.purge_id)
                .expect("recovered purge progress")
                .state,
            PurgeJournalState::Succeeded
        );
        assert!(
            library
                .unfinished_purge_cleanups(None, 1)
                .expect("unfinished purge inventory")
                .is_empty()
        );
        drop(library);
        drop(daemon);
    }
}
