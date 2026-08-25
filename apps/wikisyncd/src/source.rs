//! Bounded source-administration DTOs shared by direct and daemon writers.

use wikisync_core::WikiId;
use wikisync_mediawiki::ClientConfig;
use wikisync_store::Library;

use crate::OperationError;

/// Largest accepted MediaWiki Action API endpoint in bytes.
pub const MAX_SOURCE_API_ENDPOINT_BYTES: usize = 4 * 1024;
/// Largest accepted source language code in bytes.
pub const MAX_SOURCE_LANGUAGE_CODE_BYTES: usize = 64;

/// One source operation shared by direct and daemon writer paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceAdministration {
    /// Validates and registers a MediaWiki Action API endpoint.
    Add {
        /// HTTPS endpoint, except that loopback HTTP remains valid for fixtures.
        api_endpoint: String,
        /// Non-empty MediaWiki language code stored with the source.
        language_code: String,
    },
    /// Removes a source only when no configuration or retained evidence uses it.
    Remove { wiki_id: WikiId },
}

/// Successful result of one source-administration operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceAdministrationOutcome {
    /// A source was registered or an identical endpoint was already registered.
    Added {
        /// Durable source identity.
        wiki_id: WikiId,
        /// Durable endpoint (which may predate an idempotent repeated request).
        api_endpoint: String,
        /// Durable language code (which may predate an idempotent repeated request).
        language_code: String,
        /// True only when this request created the durable registration.
        created: bool,
    },
    /// An unused source registration was removed.
    Removed { wiki_id: WikiId },
}

/// Applies validated source administration under direct writer ownership.
///
/// Daemon dispatch calls this same helper, so endpoint policy, bounds, idempotence,
/// and safe removal cannot diverge between direct and forwarded paths.
pub fn administer_source_direct(
    library: &mut Library,
    administration: SourceAdministration,
) -> Result<SourceAdministrationOutcome, OperationError> {
    match administration {
        SourceAdministration::Add {
            api_endpoint,
            language_code,
        } => {
            validate_registration(&api_endpoint, &language_code)?;
            let existing = library
                .wikis()
                .map_err(operation_failed)?
                .into_iter()
                .find(|source| source.api_endpoint == api_endpoint);
            let wiki_id = library
                .register_wiki(&api_endpoint, &language_code)
                .map_err(operation_failed)?;
            let stored = library
                .wiki(wiki_id)
                .map_err(operation_failed)?
                .ok_or_else(|| {
                    OperationError::failed("registered source could not be read back")
                })?;
            Ok(SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint: stored.api_endpoint,
                language_code: stored.language_code,
                created: existing.is_none(),
            })
        }
        SourceAdministration::Remove { wiki_id } => {
            library.remove_wiki(wiki_id).map_err(operation_failed)?;
            Ok(SourceAdministrationOutcome::Removed { wiki_id })
        }
    }
}

fn validate_registration(api_endpoint: &str, language_code: &str) -> Result<(), OperationError> {
    validate_text_bound(
        "source API endpoint",
        api_endpoint,
        MAX_SOURCE_API_ENDPOINT_BYTES,
    )?;
    validate_text_bound(
        "source language code",
        language_code,
        MAX_SOURCE_LANGUAGE_CODE_BYTES,
    )?;
    if api_endpoint.trim().is_empty() || language_code.trim().is_empty() {
        return Err(OperationError::failed(
            "source API endpoint and language code must be non-empty",
        ));
    }
    if language_code.chars().any(char::is_control) {
        return Err(OperationError::failed(
            "source language code must not contain control characters",
        ));
    }
    let user_agent = crate::application_user_agent().map_err(operation_failed)?;
    ClientConfig::new(api_endpoint, user_agent)
        .map(|_| ())
        .map_err(operation_failed)
}

fn validate_text_bound(field: &str, value: &str, maximum: usize) -> Result<(), OperationError> {
    if value.len() > maximum {
        Err(OperationError::failed(format!(
            "{field} is {} bytes; maximum is {maximum}",
            value.len()
        )))
    } else {
        Ok(())
    }
}

fn operation_failed(error: impl std::fmt::Display) -> OperationError {
    OperationError::failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempLibrary(PathBuf);

    impl TempLibrary {
        fn new() -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::current_dir()
                .expect("current directory")
                .join(format!(".wsd-source-{}-{unique}", std::process::id()));
            fs::create_dir(&path).expect("create temporary library");
            Self(path)
        }
    }

    impl Drop for TempLibrary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn add_is_validated_and_duplicate_endpoint_returns_durable_values() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(&temporary.0).expect("library");
        let endpoint = "https://en.wikipedia.org/w/api.php";
        let added = administer_source_direct(
            &mut library,
            SourceAdministration::Add {
                api_endpoint: endpoint.to_owned(),
                language_code: "en".to_owned(),
            },
        )
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
        assert_eq!(language_code, "en");

        let duplicate = administer_source_direct(
            &mut library,
            SourceAdministration::Add {
                api_endpoint: endpoint.to_owned(),
                language_code: "different".to_owned(),
            },
        )
        .expect("duplicate source");
        assert_eq!(
            duplicate,
            SourceAdministrationOutcome::Added {
                wiki_id,
                api_endpoint: endpoint.to_owned(),
                language_code: "en".to_owned(),
                created: false,
            }
        );
        assert_eq!(library.wikis().expect("sources").len(), 1);
    }

    #[test]
    fn invalid_registration_is_rejected_before_store_mutation() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(&temporary.0).expect("library");
        let error = administer_source_direct(
            &mut library,
            SourceAdministration::Add {
                api_endpoint: "http://example.com/w/api.php".to_owned(),
                language_code: "en".to_owned(),
            },
        )
        .expect_err("remote HTTP must fail endpoint policy");
        assert!(error.message().contains("HTTPS"));
        assert!(library.wikis().expect("sources").is_empty());

        let error = administer_source_direct(
            &mut library,
            SourceAdministration::Add {
                api_endpoint: "x".repeat(MAX_SOURCE_API_ENDPOINT_BYTES + 1),
                language_code: "en".to_owned(),
            },
        )
        .expect_err("oversized endpoint must fail");
        assert!(error.message().contains("maximum"));
        let error = administer_source_direct(
            &mut library,
            SourceAdministration::Add {
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                language_code: "x".repeat(MAX_SOURCE_LANGUAGE_CODE_BYTES + 1),
            },
        )
        .expect_err("oversized language must fail");
        assert!(error.message().contains("maximum"));
        assert!(library.wikis().expect("sources").is_empty());
    }

    #[test]
    fn in_use_source_removal_fails_without_mutation() {
        let temporary = TempLibrary::new();
        let mut library = Library::open(&temporary.0).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Used source")
            .expect("collection");

        let error =
            administer_source_direct(&mut library, SourceAdministration::Remove { wiki_id })
                .expect_err("in-use source must not be removed");
        assert!(error.message().contains("still in use"));
        assert!(library.wiki(wiki_id).expect("source").is_some());
        assert!(
            library
                .collection(collection_id)
                .expect("collection")
                .is_some()
        );
    }
}
