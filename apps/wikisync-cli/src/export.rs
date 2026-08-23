use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Value, json};
use wikisync_content::{OutputKind, ThumbnailLimits, transform, validate_thumbnail};
use wikisync_core::{CollectionId, MAX_THUMBNAILS_PER_REVISION, PageId, RevisionId, WikiId};
use wikisync_mediawiki::ClientConfig;
use wikisync_store::{
    Library, ObjectId, StoreError, StoredPage, StoredRevision, StoredRevisionMedia, StoredWiki,
};

const MAX_EXPORT_ARTICLES: usize = 10_000;
const MAX_EXPORT_CANONICAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPORT_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXPORT_MEDIA_PLACEMENTS: usize =
    MAX_EXPORT_ARTICLES * MAX_THUMBNAILS_PER_REVISION as usize;
const MAX_EXISTING_OUTPUT_ENTRIES: usize = MAX_EXPORT_ARTICLES + MAX_EXPORT_MEDIA_PLACEMENTS + 10;
const EXPORT_SCHEMA: &str = "wikisync-current-export-v2";
const HISTORICAL_EXPORT_SCHEMA: &str = "wikisync-historical-export-v2";
const CONTENT_HASH_ALGORITHM: &str = "wikisync-object-v1/domain-separated-blake3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportFormat {
    Markdown,
    Text,
}

/// A durable archive point used to generate a historical time-slice export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExportAt {
    /// An RFC 3339 instant. The original spelling is retained in the manifest.
    Timestamp(String),
    /// A captured revision whose timestamp anchors the time slice.
    Revision(RevisionId),
}

impl ExportAt {
    pub(crate) fn parse(value: &str) -> Result<Self, ExportError> {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            let raw = value.parse::<u64>().map_err(|_| {
                ExportError::message("--at revision ID must be a positive 64-bit integer")
            })?;
            return RevisionId::new(raw).map(Self::Revision).map_err(|error| {
                ExportError::message(format!("invalid --at revision ID: {error}"))
            });
        }
        parse_rfc3339(value).map_err(|error| {
            ExportError::message(format!(
                "--at must be a positive revision ID or RFC 3339 timestamp: {error}"
            ))
        })?;
        Ok(Self::Timestamp(value.to_owned()))
    }
}

impl ExportFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Text => "txt",
        }
    }

    const fn output_kind(self) -> OutputKind {
        match self {
            Self::Markdown => OutputKind::Markdown,
            Self::Text => OutputKind::PlainText,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExportSummary {
    pub(crate) output: PathBuf,
    pub(crate) article_count: usize,
    pub(crate) uncaptured_page_count: usize,
    pub(crate) canonical_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct ExportError(String);

impl ExportError {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for ExportError {}

impl From<io::Error> for ExportError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<StoreError> for ExportError {
    fn from(error: StoreError) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for ExportError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

struct Article {
    page: StoredPage,
    revision: StoredRevision,
    wiki: StoredWiki,
    relative_path: String,
    media: Vec<ExportMedia>,
}

struct ExportMedia {
    stored: StoredRevisionMedia,
    relative_path: String,
}

struct ExportSelection {
    pages: BTreeMap<(WikiId, PageId), StoredPage>,
    scope: Value,
}

#[derive(Clone, Copy)]
struct ExportTotals {
    uncaptured_page_count: usize,
    canonical_bytes: u64,
}

#[derive(Clone, Copy)]
struct ExportTarget<'a> {
    output_name: &'a str,
    schema: &'static str,
    selector: Option<&'a Value>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ParsedTimestamp {
    unix_seconds: i64,
    nanosecond: u32,
}

pub(crate) fn run(
    library: &Library,
    format: ExportFormat,
    selected_collection: Option<CollectionId>,
) -> Result<ExportSummary, ExportError> {
    let selection = selected_pages(library, selected_collection)?;
    run_selected(
        library,
        format,
        selection,
        RevisionSelection::Current,
        "current".to_owned(),
        EXPORT_SCHEMA,
        None,
    )
}

/// Generates a historical slice without replacing the maintained current export.
pub(crate) fn run_at(
    library: &Library,
    format: ExportFormat,
    selected_collection: Option<CollectionId>,
    at: &ExportAt,
) -> Result<ExportSummary, ExportError> {
    let scope_name = selected_collection.map_or_else(
        || "library".to_owned(),
        |collection_id| format!("collection-{}", collection_id.get()),
    );
    let selection = selected_pages(library, selected_collection)?;
    let (cutoff, point_name, selector) = resolve_historical_cutoff(library, &selection, at)?;
    let output_name = format!("{point_name}-{}-{scope_name}", format.as_str());
    run_selected(
        library,
        format,
        selection,
        RevisionSelection::At(cutoff),
        output_name,
        HISTORICAL_EXPORT_SCHEMA,
        Some(selector),
    )
}

#[derive(Clone, Copy)]
enum RevisionSelection {
    Current,
    At(ParsedTimestamp),
}

fn run_selected(
    library: &Library,
    format: ExportFormat,
    selection: ExportSelection,
    revision_selection: RevisionSelection,
    output_name: String,
    schema: &'static str,
    selector: Option<Value>,
) -> Result<ExportSummary, ExportError> {
    let pages = selection.pages;
    if pages.len() > MAX_EXPORT_ARTICLES {
        return Err(ExportError::message(format!(
            "export selects {} pages, exceeding the bounded limit of {MAX_EXPORT_ARTICLES}; export a smaller collection",
            pages.len()
        )));
    }

    let wikis = library
        .wikis()?
        .into_iter()
        .map(|wiki| (wiki.wiki_id, wiki))
        .collect::<BTreeMap<_, _>>();
    let mut articles = Vec::with_capacity(pages.len());
    let mut canonical_bytes = 0_u64;
    let mut uncaptured_page_count = 0_usize;
    let mut used_paths = BTreeSet::new();
    let mut media_paths = BTreeMap::<ObjectId, String>::new();
    let mut media_placement_count = 0_usize;

    for page in pages.into_values() {
        let revision = match revision_selection {
            RevisionSelection::Current => current_revision(library, &page)?,
            RevisionSelection::At(cutoff) => revision_at(library, &page, cutoff)?,
        };
        let Some(revision) = revision else {
            uncaptured_page_count += 1;
            continue;
        };
        if revision.page_id != page.page_id {
            return Err(ExportError::message(format!(
                "corrupt library: revision {} belongs to page {}, not page {}",
                revision.revision_id, revision.page_id, page.page_id
            )));
        }
        canonical_bytes = canonical_bytes
            .checked_add(revision.source_size)
            .ok_or_else(|| ExportError::message("export canonical byte count overflowed"))?;
        if canonical_bytes > MAX_EXPORT_CANONICAL_BYTES {
            return Err(ExportError::message(format!(
                "export contains more than {MAX_EXPORT_CANONICAL_BYTES} canonical bytes; export a smaller collection"
            )));
        }
        let wiki = wikis.get(&page.wiki_id).cloned().ok_or_else(|| {
            ExportError::message(format!(
                "corrupt library: page {} refers to missing wiki {}",
                page.page_id, page.wiki_id
            ))
        })?;
        ClientConfig::new(&wiki.api_endpoint, "WikiSyncer offline export validation").map_err(
            |error| {
                ExportError::message(format!(
                    "wiki {} has an unsafe or invalid source endpoint: {error}",
                    wiki.wiki_id
                ))
            },
        )?;
        let stem = format!("{}-{}", page.page_id, safe_slug(page.title.as_str()));
        let mut relative_path = format!("articles/{stem}.{}", format.extension());
        if !used_paths.insert(relative_path.clone()) {
            relative_path = format!(
                "articles/{stem}-wiki-{}.{}",
                page.wiki_id,
                format.extension()
            );
            if !used_paths.insert(relative_path.clone()) {
                return Err(ExportError::message(
                    "export could not assign a unique safe article path",
                ));
            }
        }
        let media = library
            .revision_media(page.wiki_id, revision.revision_id)?
            .into_iter()
            .map(|stored| {
                media_placement_count = media_placement_count.checked_add(1).ok_or_else(|| {
                    ExportError::message("export media placement count overflowed")
                })?;
                if media_placement_count > MAX_EXPORT_MEDIA_PLACEMENTS {
                    return Err(ExportError::message(format!(
                        "export contains more than {MAX_EXPORT_MEDIA_PLACEMENTS} media placements"
                    )));
                }
                let relative_path = media_paths
                    .entry(stored.content_object_id)
                    .or_insert_with(|| media_relative_path(&stored))
                    .clone();
                Ok(ExportMedia {
                    stored,
                    relative_path,
                })
            })
            .collect::<Result<Vec<_>, ExportError>>()?;
        articles.push(Article {
            page,
            revision,
            wiki,
            relative_path,
            media,
        });
    }

    let totals = ExportTotals {
        uncaptured_page_count,
        canonical_bytes,
    };
    let target = ExportTarget {
        output_name: &output_name,
        schema,
        selector: selector.as_ref(),
    };
    write_export(library, format, &selection.scope, &articles, totals, target)?;
    Ok(ExportSummary {
        output: library.root().join("exports").join(output_name),
        article_count: articles.len(),
        uncaptured_page_count,
        canonical_bytes,
    })
}

fn current_revision(
    library: &Library,
    page: &StoredPage,
) -> Result<Option<StoredRevision>, ExportError> {
    let Some(revision_id) = page.current_revision_id else {
        return Ok(None);
    };
    library
        .revision(page.wiki_id, revision_id)?
        .ok_or_else(|| {
            ExportError::message(format!(
                "corrupt library: page {} points to missing revision {revision_id}",
                page.page_id
            ))
        })
        .map(Some)
}

fn revision_at(
    library: &Library,
    page: &StoredPage,
    cutoff: ParsedTimestamp,
) -> Result<Option<StoredRevision>, ExportError> {
    let canonical_cutoff = canonical_mediawiki_timestamp(cutoff.unix_seconds)?;
    library
        .newest_revision_for_page_at_or_before(page.wiki_id, page.page_id, &canonical_cutoff)
        .map_err(Into::into)
}

fn canonical_mediawiki_timestamp(unix_seconds: i64) -> Result<String, ExportError> {
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let shifted_days = days.checked_add(719_468).ok_or_else(|| {
        ExportError::message("historical export timestamp is outside the supported calendar")
    })?;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1..=9_999).contains(&year) {
        return Err(ExportError::message(
            "historical export timestamp is outside years 0001 through 9999",
        ));
    }
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn selected_pages(
    library: &Library,
    selected_collection: Option<CollectionId>,
) -> Result<ExportSelection, ExportError> {
    let collections = library.collections()?;
    let selected = if let Some(collection_id) = selected_collection {
        let collection = collections
            .iter()
            .find(|collection| collection.collection_id == collection_id)
            .ok_or_else(|| {
                ExportError::message(format!("collection {collection_id} was not found"))
            })?;
        vec![collection]
    } else {
        collections.iter().collect()
    };
    let scope = selected_collection.map_or_else(
        || json!({ "kind": "library", "collection_count": collections.len() }),
        |collection_id| json!({ "kind": "collection", "collection_id": collection_id.get() }),
    );
    let mut pages = BTreeMap::new();
    for collection in selected {
        for page in library.collection_pages(collection.wiki_id, collection.collection_id)? {
            pages.entry((page.wiki_id, page.page_id)).or_insert(page);
            if pages.len() > MAX_EXPORT_ARTICLES {
                return Err(ExportError::message(format!(
                    "export selects more than {MAX_EXPORT_ARTICLES} distinct pages; export a smaller collection"
                )));
            }
        }
    }
    Ok(ExportSelection { pages, scope })
}

fn resolve_historical_cutoff(
    library: &Library,
    selection: &ExportSelection,
    at: &ExportAt,
) -> Result<(ParsedTimestamp, String, Value), ExportError> {
    match at {
        ExportAt::Timestamp(value) => {
            let cutoff = parse_rfc3339(value).map_err(|error| {
                ExportError::message(format!("invalid historical export timestamp: {error}"))
            })?;
            let output_name = timestamp_output_name(cutoff);
            Ok((
                cutoff,
                output_name,
                json!({
                    "kind": "timestamp",
                    "requested": value,
                    "unix_seconds": cutoff.unix_seconds,
                    "nanosecond": cutoff.nanosecond,
                }),
            ))
        }
        ExportAt::Revision(revision_id) => {
            let matches = library
                .revisions_by_id(*revision_id)?
                .into_iter()
                .filter(|(wiki_id, revision)| {
                    selection.pages.contains_key(&(*wiki_id, revision.page_id))
                })
                .collect::<Vec<_>>();
            let [(wiki_id, revision)] = matches.as_slice() else {
                return if matches.is_empty() {
                    Err(ExportError::message(format!(
                        "revision {revision_id} was not found among the selected pages"
                    )))
                } else {
                    Err(ExportError::message(format!(
                        "revision {revision_id} is ambiguous across the selected wikis; select a single collection"
                    )))
                };
            };
            let cutoff = parse_rfc3339(&revision.timestamp).map_err(|error| {
                ExportError::message(format!(
                    "corrupt library: revision {revision_id} has invalid timestamp {:?}: {error}",
                    revision.timestamp
                ))
            })?;
            Ok((
                cutoff,
                format!("at-revision-{}-{revision_id}", wiki_id.get()),
                json!({
                    "kind": "revision",
                    "revision_id": revision_id.get(),
                    "wiki_id": wiki_id.get(),
                    "revision_time": revision.timestamp,
                    "unix_seconds": cutoff.unix_seconds,
                    "nanosecond": cutoff.nanosecond,
                }),
            ))
        }
    }
}

fn timestamp_output_name(timestamp: ParsedTimestamp) -> String {
    if timestamp.nanosecond == 0 {
        format!("at-time-unix-{}", timestamp.unix_seconds)
    } else {
        format!(
            "at-time-unix-{}-{:09}",
            timestamp.unix_seconds, timestamp.nanosecond
        )
    }
}

fn write_export(
    library: &Library,
    format: ExportFormat,
    scope: &Value,
    articles: &[Article],
    totals: ExportTotals,
    target: ExportTarget<'_>,
) -> Result<(), ExportError> {
    let exports = library.root().join("exports");
    prepare_exports_directory(&exports)?;
    let output = exports.join(target.output_name);
    validate_replaceable_output(&output, target.output_name)?;
    let staging = create_staging_directory(&exports, target.output_name)?;
    let build_result =
        build_staged_export(library, &staging, format, scope, articles, totals, target);
    if let Err(error) = build_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = install_staged_export(&exports, &staging, &output, target.output_name) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(())
}

fn build_staged_export(
    library: &Library,
    staging: &Path,
    format: ExportFormat,
    scope: &Value,
    articles: &[Article],
    totals: ExportTotals,
    target: ExportTarget<'_>,
) -> Result<(), ExportError> {
    let article_directory = staging.join("articles");
    create_private_directory(&article_directory)?;
    let media_directory = staging.join("media");
    let has_media = articles.iter().any(|article| !article.media.is_empty());
    if has_media {
        create_private_directory(&media_directory)?;
    }
    let mut index = private_new_file(&staging.join("index.jsonl"))?;
    let mut output_bytes = 0_u64;
    let mut maximum_capture_time = None;
    let mut media_bytes = 0_u64;
    let mut media_placement_count = 0_usize;
    let mut written_media = BTreeSet::new();

    for article in articles {
        let source = library.read_object(article.revision.content_object_id)?;
        let source = String::from_utf8(source).map_err(|_| {
            ExportError::message(format!(
                "corrupt library: revision {} source is not valid UTF-8",
                article.revision.revision_id
            ))
        })?;
        let derived = transform(&source, format.output_kind());
        let source_url = source_url(
            &article.wiki.api_endpoint,
            article.page.title.as_str(),
            article.revision.revision_id.get(),
        );
        let metadata = article_metadata(
            article,
            format,
            &source_url,
            derived.transformer_version.as_str(),
        );
        for media in &article.media {
            media_placement_count += 1;
            maximum_capture_time = Some(
                maximum_capture_time.map_or(media.stored.captured_at, |current: u64| {
                    current.max(media.stored.captured_at)
                }),
            );
            if written_media.insert(media.stored.content_object_id) {
                let bytes = verified_export_media_bytes(library, &media.stored)?;
                let length = u64::try_from(bytes.len()).expect("usize fits in u64");
                media_bytes = media_bytes
                    .checked_add(length)
                    .ok_or_else(|| ExportError::message("export media byte count overflowed"))?;
                output_bytes = output_bytes
                    .checked_add(length)
                    .ok_or_else(|| ExportError::message("export output byte count overflowed"))?;
                if output_bytes > MAX_EXPORT_OUTPUT_BYTES {
                    return Err(ExportError::message(format!(
                        "derived export exceeds the bounded output limit of {MAX_EXPORT_OUTPUT_BYTES} bytes"
                    )));
                }
                write_private_file(&staging.join(&media.relative_path), &bytes)?;
            }
        }
        let rendered = render_article(format, &metadata, &derived.body, article);
        output_bytes = output_bytes
            .checked_add(u64::try_from(rendered.len()).expect("usize fits in u64"))
            .ok_or_else(|| ExportError::message("export output byte count overflowed"))?;
        if output_bytes > MAX_EXPORT_OUTPUT_BYTES {
            return Err(ExportError::message(format!(
                "derived export exceeds the bounded output limit of {MAX_EXPORT_OUTPUT_BYTES} bytes"
            )));
        }
        write_private_file(&staging.join(&article.relative_path), rendered.as_bytes())?;
        serde_json::to_writer(&mut index, &metadata)?;
        index.write_all(b"\n")?;
        maximum_capture_time = Some(
            maximum_capture_time.map_or(article.revision.captured_at, |current: u64| {
                current.max(article.revision.captured_at)
            }),
        );
    }
    index.sync_all()?;

    let mut manifest = json!({
        "article_count": articles.len(),
        "canonical_source_bytes": totals.canonical_bytes,
        "content_hash_algorithm": CONTENT_HASH_ALGORITHM,
        "format": format.as_str(),
        "maximum_capture_time_unix": maximum_capture_time,
        "media_bytes": media_bytes,
        "media_object_count": written_media.len(),
        "media_placement_count": media_placement_count,
        "schema": target.schema,
        "schema_evolution": "v2-additive-attributed-local-media",
        "schema_predecessor": if target.schema == EXPORT_SCHEMA {
            "wikisync-current-export-v1"
        } else {
            "wikisync-historical-export-v1"
        },
        "scope": scope,
        "transformer_version": format.output_kind().transformer_version().as_str(),
        "uncaptured_page_count": totals.uncaptured_page_count,
    });
    if let Some(selector) = target.selector {
        manifest["at"] = selector.clone();
    }
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_bytes.push(b'\n');
    write_private_file(&staging.join("manifest.json"), &manifest_bytes)?;
    sync_directory(&article_directory)?;
    if has_media {
        sync_directory(&media_directory)?;
    }
    sync_directory(staging)?;
    Ok(())
}

fn verified_export_media_bytes(
    library: &Library,
    media: &StoredRevisionMedia,
) -> Result<Vec<u8>, ExportError> {
    let bytes = library.read_object(media.content_object_id)?;
    let pixels = u64::from(media.width)
        .checked_mul(u64::from(media.height))
        .ok_or_else(|| ExportError::message("captured media dimensions overflowed"))?;
    let limits = ThumbnailLimits {
        max_encoded_bytes: u64::try_from(bytes.len())
            .map_err(|_| ExportError::message("captured media length overflowed"))?,
        max_width: media.width,
        max_height: media.height,
        max_pixels: pixels,
        max_decoded_bytes: pixels
            .checked_mul(8)
            .ok_or_else(|| ExportError::message("captured media allocation overflowed"))?,
    };
    let validated =
        validate_thumbnail(&bytes, media.mime_type.as_str(), &limits).map_err(|_| {
            ExportError::message(format!(
                "captured media for revision {} placement {} failed passive-raster validation",
                media.revision_id, media.placement_index
            ))
        })?;
    if validated.width != media.width || validated.height != media.height {
        return Err(ExportError::message(format!(
            "captured media for revision {} placement {} disagrees with its dimensions",
            media.revision_id, media.placement_index
        )));
    }
    Ok(bytes)
}

fn article_metadata(
    article: &Article,
    format: ExportFormat,
    source_url: &str,
    transformer_version: &str,
) -> Value {
    let media = article.media.iter().map(media_metadata).collect::<Vec<_>>();
    json!({
        "author": article.revision.author,
        "capture_time_unix": article.revision.captured_at,
        "content_hash": article.revision.content_object_id.to_string(),
        "content_hash_algorithm": CONTENT_HASH_ALGORITHM,
        "format": format.as_str(),
        "media": media,
        "page_id": article.page.page_id.get(),
        "relative_path": article.relative_path,
        "revision_id": article.revision.revision_id.get(),
        "revision_time": article.revision.timestamp,
        "source_api_endpoint": article.wiki.api_endpoint,
        "source_url": source_url,
        "title": article.page.title.as_str(),
        "transformer_version": transformer_version,
        "wiki": article.wiki.language_code,
        "wiki_id": article.wiki.wiki_id.get(),
    })
}

fn media_metadata(media: &ExportMedia) -> Value {
    json!({
        "alt_text": media.stored.alt_text,
        "attribution": media.stored.attribution,
        "author": media.stored.author,
        "caption": media.stored.caption,
        "capture_time_unix": media.stored.captured_at,
        "content_hash": media.stored.content_object_id.to_string(),
        "description_url": media.stored.description_url,
        "file_title": media.stored.file_title.as_str(),
        "height": media.stored.height,
        "license_name": media.stored.license_name,
        "license_url": media.stored.license_url,
        "mime_type": media.stored.mime_type.as_str(),
        "original_url": media.stored.original_url,
        "placement_index": media.stored.placement_index,
        "placement_kind": media.stored.placement_kind.as_str(),
        "relative_path": media.relative_path,
        "source_media_id": media.stored.media_id.get(),
        "source_sha1": media.stored.source_sha1,
        "width": media.stored.width,
    })
}

fn render_article(format: ExportFormat, metadata: &Value, body: &str, article: &Article) -> String {
    let source_url = metadata["source_url"].as_str().expect("string metadata");
    let author = article
        .revision
        .author
        .as_deref()
        .unwrap_or("not available");
    match format {
        ExportFormat::Markdown => {
            let mut rendered = String::from("---\n");
            for key in [
                "wiki",
                "wiki_id",
                "page_id",
                "title",
                "revision_id",
                "revision_time",
                "source_url",
                "source_api_endpoint",
                "capture_time_unix",
                "content_hash",
                "content_hash_algorithm",
                "transformer_version",
            ] {
                rendered.push_str(key);
                rendered.push_str(": ");
                rendered.push_str(&metadata[key].to_string());
                rendered.push('\n');
            }
            rendered.push_str("---\n\n");
            rendered.push_str(body);
            if !body.ends_with('\n') && !body.is_empty() {
                rendered.push('\n');
            }
            render_markdown_media(&mut rendered, &article.media);
            rendered.push_str("\n## Source and attribution\n\n");
            rendered.push_str(&format!(
                "Source: [{}]({source_url}), revision {} ({}).\n\n",
                markdown_text(article.page.title.as_str()),
                article.revision.revision_id,
                article.revision.timestamp
            ));
            rendered.push_str(&format!("Revision author: {}.\n\n", markdown_text(author)));
            rendered.push_str(
                "The source wiki's license and attribution requirements apply; license metadata is not available in this library export.\n",
            );
            rendered
        }
        ExportFormat::Text => {
            let mut rendered = format!(
                "Title: {}\nWiki: {} (local ID {})\nPage ID: {}\nRevision ID: {}\nRevision time: {}\nSource URL: {source_url}\nSource API endpoint: {}\nCapture time (Unix): {}\nContent hash: {}\nContent hash algorithm: {CONTENT_HASH_ALGORITHM}\nTransformer: {}\n\n",
                article.page.title,
                article.wiki.language_code,
                article.wiki.wiki_id,
                article.page.page_id,
                article.revision.revision_id,
                article.revision.timestamp,
                article.wiki.api_endpoint,
                article.revision.captured_at,
                article.revision.content_object_id,
                format.output_kind().transformer_version(),
            );
            rendered.push_str(body);
            if !body.ends_with('\n') && !body.is_empty() {
                rendered.push('\n');
            }
            render_text_media(&mut rendered, &article.media);
            rendered.push_str("\nSOURCE AND ATTRIBUTION\n");
            rendered.push_str(&format!(
                "Source: {} (revision {}, {}).\nRevision author: {author}.\n",
                article.page.title, article.revision.revision_id, article.revision.timestamp
            ));
            rendered.push_str(&format!("Permanent source URL: {source_url}\n"));
            rendered.push_str(
                "The source wiki's license and attribution requirements apply; license metadata is not available in this library export.\n",
            );
            rendered
        }
    }
}

fn render_markdown_media(rendered: &mut String, media: &[ExportMedia]) {
    if media.is_empty() {
        return;
    }
    rendered.push_str("\n## Captured media\n\n");
    for item in media {
        let caption = media_caption(&item.stored);
        let alternative = media_alternative(&item.stored);
        rendered.push_str(&format!(
            "![{}](../{})\n\n",
            markdown_text(alternative),
            item.relative_path
        ));
        rendered.push_str(&format!("**{}**  \n", markdown_text(caption)));
        rendered.push_str(&format!(
            "Artist/creator: {}. Credit: {}.  \n",
            markdown_text(&item.stored.author),
            markdown_text(&item.stored.attribution)
        ));
        rendered.push_str(&format!(
            "{} License: {}. Dimensions: {} × {} px.  \n",
            markdown_metadata_link("Source description", &item.stored.description_url),
            markdown_metadata_link(
                &item.stored.license_name,
                item.stored.license_url.as_deref().unwrap_or("")
            ),
            item.stored.width,
            item.stored.height
        ));
        rendered.push_str(&format!(
            "Observed rendition: {}. Captured locally at Unix timestamp {} as {} placement {}. Upstream file hash: `{}`. Local content object: `{}`.\n\n",
            markdown_metadata_link("source URL", &item.stored.original_url),
            item.stored.captured_at,
            item.stored.placement_kind.as_str(),
            item.stored.placement_index,
            markdown_text(&item.stored.source_sha1),
            item.stored.content_object_id,
        ));
    }
}

fn render_text_media(rendered: &mut String, media: &[ExportMedia]) {
    if media.is_empty() {
        return;
    }
    rendered.push_str("\nCAPTURED MEDIA\n");
    for item in media {
        rendered.push_str(&format!(
            "\nMedia {}: {}\nLocal file: ../{}\nAlternative text: {}\nArtist/creator: {}\nCredit: {}\nSource description: {}\nLicense: {}{}\nDimensions: {} x {} px\nObserved rendition URL: {}\nCaptured locally (Unix): {}\nPlacement: {} {}\nUpstream file hash: {}\nLocal content object: {}\n",
            item.stored.placement_index + 1,
            media_caption(&item.stored),
            item.relative_path,
            media_alternative(&item.stored),
            item.stored.author,
            item.stored.attribution,
            item.stored.description_url,
            item.stored.license_name,
            item.stored
                .license_url
                .as_deref()
                .map_or_else(String::new, |url| format!(" ({url})")),
            item.stored.width,
            item.stored.height,
            item.stored.original_url,
            item.stored.captured_at,
            item.stored.placement_kind.as_str(),
            item.stored.placement_index,
            item.stored.source_sha1,
            item.stored.content_object_id,
        ));
    }
}

fn media_caption(media: &StoredRevisionMedia) -> &str {
    media
        .caption
        .as_deref()
        .or(media.alt_text.as_deref())
        .unwrap_or_else(|| media.file_title.as_str())
}

fn media_alternative(media: &StoredRevisionMedia) -> &str {
    media
        .alt_text
        .as_deref()
        .or(media.caption.as_deref())
        .unwrap_or_else(|| media.file_title.as_str())
}

fn markdown_metadata_link(label: &str, url: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        format!("[{}]({})", markdown_text(label), markdown_url(url))
    } else if url.is_empty() {
        markdown_text(label)
    } else {
        format!("{}: {}", markdown_text(label), markdown_text(url))
    }
}

fn markdown_url(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

fn media_relative_path(media: &StoredRevisionMedia) -> String {
    let extension = match media.mime_type.as_str() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        _ => unreachable!("stored media MIME type is a closed passive-raster enum"),
    };
    format!(
        "media/{}.{}",
        media.content_object_id.to_string().replace(':', "-"),
        extension
    )
}

fn safe_slug(title: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in title.chars() {
        if slug.len() >= 64 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() && slug.len() < 64 {
                slug.push('-');
            }
            if slug.len() < 64 {
                slug.push(character.to_ascii_lowercase());
            }
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        "article".to_owned()
    } else {
        slug
    }
}

fn source_url(endpoint: &str, title: &str, revision_id: u64) -> String {
    let endpoint = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    let base = endpoint
        .strip_suffix("api.php")
        .map_or(endpoint.to_owned(), |prefix| format!("{prefix}index.php"));
    format!(
        "{base}?title={}&oldid={revision_id}",
        percent_encode_query_value(title)
    )
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

fn markdown_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn parse_rfc3339(value: &str) -> Result<ParsedTimestamp, &'static str> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return Err("expected YYYY-MM-DDTHH:MM:SSZ or an equivalent numeric offset");
    }
    let year = decimal(bytes, 0, 4)? as i64;
    let month = decimal(bytes, 5, 7)?;
    let day = decimal(bytes, 8, 10)?;
    let hour = decimal(bytes, 11, 13)?;
    let minute = decimal(bytes, 14, 16)?;
    let second = decimal(bytes, 17, 19)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("date or time component is out of range");
    }

    let mut cursor = 19;
    let mut nanosecond = 0_u32;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        let digits = cursor - fraction_start;
        if digits == 0 {
            return Err("fractional seconds must contain at least one digit");
        }
        let retained = digits.min(9);
        nanosecond = decimal(bytes, fraction_start, fraction_start + retained)?;
        for _ in retained..9 {
            nanosecond *= 10;
        }
        if bytes[fraction_start + retained..cursor]
            .iter()
            .any(|byte| *byte != b'0')
        {
            return Err("fractional seconds finer than nanoseconds are not supported");
        }
    }

    let offset_seconds = match bytes.get(cursor) {
        Some(b'Z') if cursor + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-'))
            if cursor + 6 == bytes.len() && bytes.get(cursor + 3) == Some(&b':') =>
        {
            let offset_hour = decimal(bytes, cursor + 1, cursor + 3)?;
            let offset_minute = decimal(bytes, cursor + 4, cursor + 6)?;
            if offset_hour > 23 || offset_minute > 59 {
                return Err("UTC offset is out of range");
            }
            let offset = i64::from(offset_hour * 3_600 + offset_minute * 60);
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return Err("timestamp must end in Z or a numeric UTC offset"),
    };

    let days = days_from_civil(year, month, day);
    let local_seconds = days
        .checked_mul(86_400)
        .and_then(|seconds| seconds.checked_add(i64::from(hour * 3_600)))
        .and_then(|seconds| seconds.checked_add(i64::from(minute * 60)))
        .and_then(|seconds| seconds.checked_add(i64::from(second)))
        .ok_or("timestamp is outside the supported range")?;
    let unix_seconds = local_seconds
        .checked_sub(offset_seconds)
        .ok_or("timestamp is outside the supported range")?;
    Ok(ParsedTimestamp {
        unix_seconds,
        nanosecond,
    })
}

fn decimal(bytes: &[u8], start: usize, end: usize) -> Result<u32, &'static str> {
    let digits = bytes
        .get(start..end)
        .ok_or("timestamp ended before a required component")?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err("timestamp contains a non-numeric component");
    }
    Ok(digits
        .iter()
        .fold(0_u32, |value, byte| value * 10 + u32::from(byte - b'0')))
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's civil-calendar conversion, shifted to the Unix epoch.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn prepare_exports_directory(exports: &Path) -> Result<(), ExportError> {
    match fs::symlink_metadata(exports) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExportError::message(
            "refusing to write export through a symbolic link at exports",
        )),
        Ok(metadata) if !metadata.is_dir() => Err(ExportError::message(
            "refusing to replace non-directory exports path",
        )),
        Ok(_) => {
            restrict_directory_permissions(exports)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_directory(exports),
        Err(error) => Err(error.into()),
    }
}

fn validate_replaceable_output(output: &Path, output_name: &str) -> Result<(), ExportError> {
    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExportError::message(format!(
            "refusing to replace symbolic link at exports/{output_name}"
        ))),
        Ok(metadata) if !metadata.is_dir() => Err(ExportError::message(format!(
            "refusing to replace non-directory exports/{output_name}"
        ))),
        Ok(_) => assert_no_symlinks(output, output_name),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn assert_no_symlinks(root: &Path, output_name: &str) -> Result<(), ExportError> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            entries += 1;
            if entries > MAX_EXISTING_OUTPUT_ENTRIES {
                return Err(ExportError::message(format!(
                    "existing exports/{output_name} contains too many entries to replace safely"
                )));
            }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(ExportError::message(format!(
                    "refusing to replace exports/{output_name} containing symbolic link {}",
                    entry.path().display()
                )));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn create_staging_directory(exports: &Path, output_name: &str) -> Result<PathBuf, ExportError> {
    for attempt in 0..100_u32 {
        let path = exports.join(format!(
            ".{output_name}-stage-{}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                restrict_directory_permissions(&path)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ExportError::message(
        "could not allocate a private export staging directory",
    ))
}

fn install_staged_export(
    exports: &Path,
    staging: &Path,
    output: &Path,
    output_name: &str,
) -> Result<(), ExportError> {
    if !output.exists() {
        fs::rename(staging, output)?;
        sync_directory(exports)?;
        return Ok(());
    }
    let backup = exports.join(format!(".{output_name}-backup-{}", std::process::id()));
    if fs::symlink_metadata(&backup).is_ok() {
        return Err(ExportError::message(format!(
            "refusing to overwrite unexpected export backup {}",
            backup.display()
        )));
    }
    fs::rename(output, &backup)?;
    if let Err(error) = fs::rename(staging, output) {
        let restore = fs::rename(&backup, output);
        return match restore {
            Ok(()) => Err(error.into()),
            Err(restore_error) => Err(ExportError::message(format!(
                "failed to install export ({error}) and restore prior export ({restore_error}); prior output remains at {}",
                backup.display()
            ))),
        };
    }
    sync_directory(exports)?;
    fs::remove_dir_all(&backup)?;
    sync_directory(exports)?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), ExportError> {
    fs::create_dir(path)?;
    restrict_directory_permissions(path)?;
    Ok(())
}

fn restrict_directory_permissions(path: &Path) -> Result<(), ExportError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn private_new_file(path: &Path) -> Result<File, ExportError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    let mut file = private_new_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ExportError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_source_urls_cannot_escape_the_export_tree() {
        assert_eq!(safe_slug("../../A/B: C"), "a-b-c");
        assert_eq!(safe_slug("資料"), "article");
        assert_eq!(
            source_url(
                "https://en.wikipedia.org/w/api.php",
                "A title/with? punctuation",
                42
            ),
            "https://en.wikipedia.org/w/index.php?title=A%20title%2Fwith%3F%20punctuation&oldid=42"
        );
    }

    #[test]
    fn rfc3339_parser_normalizes_offsets_and_honors_fractional_boundaries() {
        assert_eq!(
            parse_rfc3339("2026-08-19T12:00:00Z").unwrap(),
            parse_rfc3339("2026-08-19T14:00:00+02:00").unwrap()
        );
        assert_eq!(
            parse_rfc3339("1970-01-01T00:00:00Z").unwrap(),
            ParsedTimestamp {
                unix_seconds: 0,
                nanosecond: 0
            }
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T23:59:59.25Z").unwrap().nanosecond,
            250_000_000
        );
        assert!(parse_rfc3339("2026-02-29T00:00:00Z").is_err());
        assert!(parse_rfc3339("2026-08-19 12:00:00Z").is_err());
        assert!(parse_rfc3339("2026-08-19T12:00:00").is_err());
        let normalized = parse_rfc3339("2026-08-19T14:00:00.75+02:00").unwrap();
        assert_eq!(
            canonical_mediawiki_timestamp(normalized.unix_seconds).unwrap(),
            "2026-08-19T12:00:00Z"
        );
        assert_eq!(
            canonical_mediawiki_timestamp(0).unwrap(),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn at_parser_distinguishes_revision_ids_from_timestamps() {
        assert_eq!(
            ExportAt::parse("42").unwrap(),
            ExportAt::Revision(RevisionId::new(42).unwrap())
        );
        assert_eq!(
            ExportAt::parse("2026-08-19T12:00:00Z").unwrap(),
            ExportAt::Timestamp("2026-08-19T12:00:00Z".to_owned())
        );
        assert!(ExportAt::parse("0").is_err());
        assert!(ExportAt::parse("yesterday").is_err());
    }
}
