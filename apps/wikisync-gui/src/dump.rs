use std::path::Path;
use std::time::Duration;

use wikisync_core::{CollectionBudget, CollectionId, HistoryPolicy};
use wikisync_mediawiki::{DumpAcquisitionLimits, DumpDigest, DumpLimits, TrustedDumpIndex};
use wikisync_store::{Library, StoredCollectionConfiguration};
use wikisyncd::{CurrentDumpBootstrapRequest, preview_current_dump_bootstrap};

pub(crate) const INDEPENDENT_ANCHOR_NOTICE: &str = "Enter a BLAKE3 digest retained through an independent trusted channel. A legacy checksum downloaded beside the dump index is not an independent trust anchor.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DumpBootstrapForm {
    pub collection_id: String,
    pub trusted_index_url: String,
    pub trusted_index_digest: String,
    pub expected_database: String,
    pub max_index_bytes: String,
    pub max_artifact_bytes: String,
    pub max_total_artifact_bytes: String,
    pub max_artifacts: String,
    pub max_elapsed_seconds: String,
    pub max_compressed_bytes: String,
    pub max_decompressed_bytes: String,
    pub max_pages: String,
    pub max_page_xml_bytes: String,
    pub max_text_bytes: String,
}

impl Default for DumpBootstrapForm {
    fn default() -> Self {
        let acquisition = DumpAcquisitionLimits::default();
        let parser = DumpLimits::default();
        let total_artifact_bytes = acquisition
            .max_total_artifact_bytes
            .min(parser.max_compressed_bytes);
        Self {
            collection_id: String::new(),
            trusted_index_url: String::new(),
            trusted_index_digest: String::new(),
            expected_database: "enwiki".to_owned(),
            max_index_bytes: acquisition.max_index_bytes.to_string(),
            max_artifact_bytes: acquisition.max_artifact_bytes.to_string(),
            max_total_artifact_bytes: total_artifact_bytes.to_string(),
            max_artifacts: acquisition.max_artifacts.to_string(),
            max_elapsed_seconds: acquisition.max_elapsed.as_secs().to_string(),
            max_compressed_bytes: parser.max_compressed_bytes.to_string(),
            max_decompressed_bytes: parser.max_decompressed_bytes.to_string(),
            max_pages: parser.max_pages.to_string(),
            max_page_xml_bytes: parser.max_page_xml_bytes.to_string(),
            max_text_bytes: parser.max_text_bytes.to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DumpBootstrapDraft {
    pub collection_id: CollectionId,
    pub trusted_index_url: String,
    pub trusted_index_digest: DumpDigest,
    pub expected_database: String,
    pub acquisition_limits: DumpAcquisitionLimits,
    pub parser_limits: DumpLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DumpBootstrapPreview {
    pub draft: DumpBootstrapDraft,
    pub wiki_id: wikisync_core::WikiId,
    pub source_endpoint: String,
    pub language_code: String,
    pub collection_name: String,
    pub collection_generation: u64,
    pub resolved_pages: usize,
    pub budget: CollectionBudget,
    pub cache_directory: String,
    pub max_concurrent_requests: u32,
    pub max_download_bytes_per_second: Option<u64>,
    pub avoid_metered_networks: bool,
}

impl DumpBootstrapForm {
    pub fn preview(&self, library_root: &Path) -> Result<DumpBootstrapPreview, String> {
        let draft = self.draft()?;
        let library = Library::open_read_only(library_root).map_err(|error| error.to_string())?;
        let service_preview = preview_current_dump_bootstrap(&library, &draft.request()?)
            .map_err(|error| error.to_string())?;
        let configuration = library
            .collection_configuration(draft.collection_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Collection configuration is unavailable.".to_owned())?;
        validate_collection(&configuration)?;
        let resolved_pages = usize::try_from(service_preview.selected_pages)
            .map_err(|_| "Resolved scope exceeds this platform's size limit.".to_owned())?;
        if let Some(maximum) = configuration.budget.maximum_pages() {
            if u64::try_from(resolved_pages).unwrap_or(u64::MAX) > maximum.get() {
                return Err(format!(
                    "Resolved scope contains {resolved_pages} pages and exceeds the hard {}-page budget.",
                    maximum.get()
                ));
            }
        }
        Ok(DumpBootstrapPreview {
            draft,
            wiki_id: service_preview.wiki_id,
            source_endpoint: service_preview.source_api_endpoint,
            language_code: service_preview.source_language_code,
            collection_name: configuration.name,
            collection_generation: configuration.generation,
            resolved_pages,
            budget: configuration.budget,
            cache_directory: service_preview.cache_directory,
            max_concurrent_requests: service_preview.max_concurrent_requests,
            max_download_bytes_per_second: service_preview.max_download_bytes_per_second,
            avoid_metered_networks: service_preview.avoid_metered_networks,
        })
    }

    fn draft(&self) -> Result<DumpBootstrapDraft, String> {
        let collection_id = CollectionId::new(parse_u64("Collection ID", &self.collection_id)?)
            .map_err(|error| error.to_string())?;
        let trusted_index_digest =
            DumpDigest::from_hex(self.trusted_index_digest.trim()).map_err(|_| {
                "Trusted index digest must be exactly 64 hexadecimal BLAKE3 digits.".to_owned()
            })?;
        let trusted_index_url = self.trusted_index_url.trim().to_owned();
        let expected_database = self.expected_database.trim().to_owned();
        TrustedDumpIndex::new(
            &trusted_index_url,
            trusted_index_digest,
            expected_database.clone(),
        )
        .map_err(|error| error.to_string())?;

        let acquisition_limits = DumpAcquisitionLimits {
            max_index_bytes: parse_usize("Maximum index bytes", &self.max_index_bytes)?,
            max_artifact_bytes: parse_u64("Maximum bytes per artifact", &self.max_artifact_bytes)?,
            max_total_artifact_bytes: parse_u64(
                "Maximum total downloaded/stored artifact bytes",
                &self.max_total_artifact_bytes,
            )?,
            max_artifacts: parse_usize("Maximum artifact count", &self.max_artifacts)?,
            max_elapsed: Duration::from_secs(parse_u64(
                "Maximum acquisition seconds",
                &self.max_elapsed_seconds,
            )?),
        };
        if acquisition_limits.max_artifact_bytes > acquisition_limits.max_total_artifact_bytes {
            return Err(
                "Maximum bytes per artifact cannot exceed the total artifact byte limit."
                    .to_owned(),
            );
        }

        let defaults = DumpLimits::default();
        let parser_limits = DumpLimits {
            max_compressed_bytes: parse_u64(
                "Maximum compressed parser bytes",
                &self.max_compressed_bytes,
            )?,
            max_decompressed_bytes: parse_u64(
                "Maximum decompressed parser bytes",
                &self.max_decompressed_bytes,
            )?,
            max_pages: parse_u64("Maximum scanned pages", &self.max_pages)?,
            max_page_xml_bytes: parse_u64("Maximum XML bytes per page", &self.max_page_xml_bytes)?,
            max_text_bytes: parse_usize("Maximum revision text bytes", &self.max_text_bytes)?,
            max_metadata_field_bytes: defaults.max_metadata_field_bytes,
            max_siteinfo_bytes: defaults.max_siteinfo_bytes,
            max_namespaces: defaults.max_namespaces,
        };
        if parser_limits.max_text_bytes as u64 > parser_limits.max_page_xml_bytes {
            return Err(
                "Maximum revision text bytes cannot exceed maximum XML bytes per page.".to_owned(),
            );
        }
        if acquisition_limits.max_total_artifact_bytes > parser_limits.max_compressed_bytes {
            return Err(
                "Maximum total artifact bytes cannot exceed the compressed parser byte limit."
                    .to_owned(),
            );
        }

        Ok(DumpBootstrapDraft {
            collection_id,
            trusted_index_url,
            trusted_index_digest,
            expected_database,
            acquisition_limits,
            parser_limits,
        })
    }
}

impl DumpBootstrapDraft {
    pub fn request(&self) -> Result<CurrentDumpBootstrapRequest, String> {
        let trusted_index = TrustedDumpIndex::new(
            &self.trusted_index_url,
            self.trusted_index_digest,
            self.expected_database.clone(),
        )
        .map_err(|error| error.to_string())?;
        CurrentDumpBootstrapRequest::new(self.collection_id, trusted_index)
            .and_then(|request| request.with_limits(self.acquisition_limits, self.parser_limits))
            .map_err(|error| error.to_string())
    }
}

fn validate_collection(configuration: &StoredCollectionConfiguration) -> Result<(), String> {
    if configuration.history_policy != HistoryPolicy::CurrentAndFuture {
        return Err(
            "Current-dump bootstrap requires a current-and-future collection history policy."
                .to_owned(),
        );
    }
    Ok(())
}

fn parse_u64(label: &str, value: &str) -> Result<u64, String> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a positive integer."))?;
    if parsed == 0 {
        return Err(format!("{label} must be greater than zero."));
    }
    Ok(parsed)
}

fn parse_usize(label: &str, value: &str) -> Result<usize, String> {
    let parsed = parse_u64(label, value)?;
    usize::try_from(parsed).map_err(|_| format!("{label} is too large on this platform."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikisync_core::{
        CollectionRemovalPolicy, CollectionRule, InclusionReason, PageId, PageTitle, TitleSelection,
    };
    use wikisync_store::{CollectionPreviewCommit, ResolvedCollectionMember};

    fn preview_fixture() -> (tempfile::TempDir, CollectionId) {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let title = PageTitle::new("Alpha").expect("title");
        let rule = CollectionRule::ExplicitTitles(
            TitleSelection::new([title.clone()]).expect("selection"),
        );
        let member = ResolvedCollectionMember {
            page_id: PageId::new(10).expect("page"),
            namespace: 0,
            title: title.clone(),
            inclusion_reason: InclusionReason::ExplicitTitle(title),
        };
        let budget = CollectionBudget::unlimited()
            .with_maximum_pages(10)
            .expect("page budget")
            .with_maximum_bytes(1_000_000)
            .expect("byte budget");
        let (collection_id, _) = library
            .create_collection_from_preview(
                wiki_id,
                "Dump fixture",
                CollectionPreviewCommit {
                    rule: &rule,
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget,
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                    members: &[member],
                    missing_titles: &[],
                    predicted_canonical_bytes: Some(5),
                },
            )
            .expect("collection");
        drop(library);
        (directory, collection_id)
    }

    fn valid_form(collection_id: CollectionId) -> DumpBootstrapForm {
        DumpBootstrapForm {
            collection_id: collection_id.get().to_string(),
            trusted_index_url: "https://dumps.wikimedia.org/enwiki/fixture/index.json".to_owned(),
            trusted_index_digest: "ab".repeat(32),
            expected_database: "enwiki".to_owned(),
            ..DumpBootstrapForm::default()
        }
    }

    #[test]
    fn preview_binds_trust_source_scope_limits_and_hard_budget() {
        let (directory, collection_id) = preview_fixture();
        let preview = valid_form(collection_id)
            .preview(directory.path())
            .expect("preview");

        assert_eq!(preview.draft.collection_id, collection_id);
        assert_eq!(preview.draft.expected_database, "enwiki");
        assert_eq!(
            preview.source_endpoint,
            "https://en.wikipedia.org/w/api.php"
        );
        assert_eq!(preview.collection_name, "Dump fixture");
        assert_eq!(preview.resolved_pages, 1);
        assert_eq!(preview.budget.maximum_pages().unwrap().get(), 10);
        assert_eq!(preview.budget.maximum_bytes().unwrap().get(), 1_000_000);
        assert_eq!(preview.cache_directory, "cache/dumps");
        assert_eq!(preview.max_concurrent_requests, 4);
        assert_eq!(preview.max_download_bytes_per_second, None);
        assert!(preview.draft.acquisition_limits.max_total_artifact_bytes > 0);
        assert!(preview.draft.parser_limits.max_decompressed_bytes > 0);
    }

    #[test]
    fn preview_rejects_missing_independent_digest_and_inconsistent_limits() {
        let (directory, collection_id) = preview_fixture();
        let mut form = valid_form(collection_id);
        form.trusted_index_digest.clear();
        assert!(
            form.preview(directory.path())
                .unwrap_err()
                .contains("64 hexadecimal")
        );

        form.trusted_index_digest = "cd".repeat(32);
        form.max_total_artifact_bytes = "100".to_owned();
        form.max_artifact_bytes = "101".to_owned();
        assert!(
            form.preview(directory.path())
                .unwrap_err()
                .contains("cannot exceed")
        );
    }
}
