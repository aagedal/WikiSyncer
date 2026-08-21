use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Value, json};
use wikisync_content::{OutputKind, transform};
use wikisync_core::{CollectionId, PageId, WikiId};
use wikisync_mediawiki::ClientConfig;
use wikisync_store::{Library, StoreError, StoredPage, StoredRevision, StoredWiki};

const MAX_EXPORT_ARTICLES: usize = 10_000;
const MAX_EXPORT_CANONICAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPORT_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXISTING_OUTPUT_ENTRIES: usize = MAX_EXPORT_ARTICLES + 8;
const EXPORT_SCHEMA: &str = "wikisync-current-export-v1";
const CONTENT_HASH_ALGORITHM: &str = "wikisync-object-v1/domain-separated-blake3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportFormat {
    Markdown,
    Text,
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
}

struct ExportSelection {
    pages: BTreeMap<(WikiId, PageId), StoredPage>,
    scope: Value,
}

pub(crate) fn run(
    library: &Library,
    format: ExportFormat,
    selected_collection: Option<CollectionId>,
) -> Result<ExportSummary, ExportError> {
    let selection = selected_pages(library, selected_collection)?;
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

    for page in pages.into_values() {
        let Some(revision_id) = page.current_revision_id else {
            uncaptured_page_count += 1;
            continue;
        };
        let revision = library
            .revision(page.wiki_id, revision_id)?
            .ok_or_else(|| {
                ExportError::message(format!(
                    "corrupt library: page {} points to missing revision {revision_id}",
                    page.page_id
                ))
            })?;
        if revision.page_id != page.page_id {
            return Err(ExportError::message(format!(
                "corrupt library: revision {revision_id} belongs to page {}, not page {}",
                revision.page_id, page.page_id
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
        articles.push(Article {
            page,
            revision,
            wiki,
            relative_path,
        });
    }

    write_export(
        library,
        format,
        &selection.scope,
        &articles,
        uncaptured_page_count,
        canonical_bytes,
    )?;
    Ok(ExportSummary {
        output: library.root().join("exports/current"),
        article_count: articles.len(),
        uncaptured_page_count,
        canonical_bytes,
    })
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

fn write_export(
    library: &Library,
    format: ExportFormat,
    scope: &Value,
    articles: &[Article],
    uncaptured_page_count: usize,
    canonical_bytes: u64,
) -> Result<(), ExportError> {
    let exports = library.root().join("exports");
    prepare_exports_directory(&exports)?;
    let current = exports.join("current");
    validate_replaceable_output(&current)?;
    let staging = create_staging_directory(&exports)?;
    let build_result = build_staged_export(
        library,
        &staging,
        format,
        scope,
        articles,
        uncaptured_page_count,
        canonical_bytes,
    );
    if let Err(error) = build_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = install_staged_export(&exports, &staging, &current) {
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
    uncaptured_page_count: usize,
    canonical_bytes: u64,
) -> Result<(), ExportError> {
    let article_directory = staging.join("articles");
    create_private_directory(&article_directory)?;
    let mut index = private_new_file(&staging.join("index.jsonl"))?;
    let mut output_bytes = 0_u64;
    let mut maximum_capture_time = None;

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

    let manifest = json!({
        "article_count": articles.len(),
        "canonical_source_bytes": canonical_bytes,
        "content_hash_algorithm": CONTENT_HASH_ALGORITHM,
        "format": format.as_str(),
        "maximum_capture_time_unix": maximum_capture_time,
        "schema": EXPORT_SCHEMA,
        "scope": scope,
        "transformer_version": format.output_kind().transformer_version().as_str(),
        "uncaptured_page_count": uncaptured_page_count,
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_bytes.push(b'\n');
    write_private_file(&staging.join("manifest.json"), &manifest_bytes)?;
    sync_directory(&article_directory)?;
    sync_directory(staging)?;
    Ok(())
}

fn article_metadata(
    article: &Article,
    format: ExportFormat,
    source_url: &str,
    transformer_version: &str,
) -> Value {
    json!({
        "author": article.revision.author,
        "capture_time_unix": article.revision.captured_at,
        "content_hash": article.revision.content_object_id.to_string(),
        "content_hash_algorithm": CONTENT_HASH_ALGORITHM,
        "format": format.as_str(),
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

fn validate_replaceable_output(current: &Path) -> Result<(), ExportError> {
    match fs::symlink_metadata(current) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExportError::message(
            "refusing to replace symbolic link at exports/current",
        )),
        Ok(metadata) if !metadata.is_dir() => Err(ExportError::message(
            "refusing to replace non-directory exports/current",
        )),
        Ok(_) => assert_no_symlinks(current),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn assert_no_symlinks(root: &Path) -> Result<(), ExportError> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            entries += 1;
            if entries > MAX_EXISTING_OUTPUT_ENTRIES {
                return Err(ExportError::message(
                    "existing exports/current contains too many entries to replace safely",
                ));
            }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(ExportError::message(format!(
                    "refusing to replace exports/current containing symbolic link {}",
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

fn create_staging_directory(exports: &Path) -> Result<PathBuf, ExportError> {
    for attempt in 0..100_u32 {
        let path = exports.join(format!(".current-stage-{}-{attempt}", std::process::id()));
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
    current: &Path,
) -> Result<(), ExportError> {
    if !current.exists() {
        fs::rename(staging, current)?;
        sync_directory(exports)?;
        return Ok(());
    }
    let backup = exports.join(format!(".current-backup-{}", std::process::id()));
    if fs::symlink_metadata(&backup).is_ok() {
        return Err(ExportError::message(format!(
            "refusing to overwrite unexpected export backup {}",
            backup.display()
        )));
    }
    fs::rename(current, &backup)?;
    if let Err(error) = fs::rename(staging, current) {
        let restore = fs::rename(&backup, current);
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
}
