//! Loopback-only routes, templates, and offline assets for the local reader.

use std::error::Error;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::Deserialize;
use wikisync_content::{DiffMode, DiffTag, diff, to_markdown, to_plain_text};
use wikisync_core::{PageId, PageTitle, RevisionId, WikiId};
use wikisync_search::{SearchIndex, SearchQuery, SqliteSearchIndex};
use wikisync_store::{Library, StoredPage, StoredRevision};

const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self' data:; \
    script-src 'none'; connect-src 'none'; font-src 'none'; form-action 'self'; \
    base-uri 'none'; frame-ancestors 'none'";
const CSS: &str = r#"
:root { color-scheme: light dark; font-family: ui-serif, Georgia, serif; line-height: 1.55; }
body { margin: 0 auto; max-width: 72rem; padding: 1.5rem; }
header { border-bottom: 1px solid #8886; margin-bottom: 2rem; padding-bottom: .8rem; }
nav a { margin-right: 1rem; }
a { color: #2769aa; }
input { font: inherit; min-width: min(28rem, 70vw); padding: .4rem; }
button { font: inherit; padding: .4rem .8rem; }
.meta, time { color: #666; font-family: ui-sans-serif, system-ui, sans-serif; font-size: .9rem; }
.notice { border-left: .25rem solid #b87900; padding-left: 1rem; }
.diff { border-collapse: collapse; font-family: ui-monospace, monospace; width: 100%; }
.diff td { padding: .15rem .5rem; vertical-align: top; white-space: pre-wrap; }
.diff .delete { background: #d733331a; }
.diff .insert { background: #2a9d4b1a; }
.diff del { background: #d7333340; }
.diff ins { background: #2a9d4b40; }
article { overflow-wrap: anywhere; }
pre { overflow-x: auto; }
table:not(.diff) { border-collapse: collapse; display: block; overflow-x: auto; }
th, td { border: 1px solid #8886; padding: .25rem .5rem; }
@media (prefers-color-scheme: dark) { a { color: #75baff; } .meta, time { color: #aaa; } }
"#;

#[derive(Clone, Debug)]
struct AppState {
    library_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct WikiQuery {
    wiki: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct SearchParameters {
    q: Option<String>,
    wiki: Option<u64>,
}

/// Builds the read-only application without binding a network listener.
///
/// Opening the library per request keeps SQLite connections out of shared async
/// state and allows the sync writer to continue using WAL mode concurrently.
pub fn router(library_root: impl AsRef<Path>) -> Router {
    let state = AppState {
        library_root: library_root.as_ref().to_path_buf(),
    };
    Router::new()
        .route("/", get(home))
        .route("/search", get(search))
        .route("/wiki/{*title}", get(article))
        .route("/page/{page_id}/history", get(history))
        .route("/revision/{revision_id}", get(revision))
        .route("/diff/{from}/{to}", get(revision_diff))
        .route("/changes", get(changes))
        .route("/collections", get(collections))
        .route("/about/source-and-integrity", get(about))
        .route("/assets/reader.css", get(stylesheet))
        .fallback(not_found)
        .with_state(state)
}

/// Serves the reader on a loopback address.
///
/// LAN binding is intentionally rejected until the planned authentication and
/// explicit-warning flow exists.
pub async fn serve(library_root: impl AsRef<Path>, address: SocketAddr) -> Result<(), ServeError> {
    if !address.ip().is_loopback() {
        return Err(ServeError::NonLoopbackAddress(address));
    }
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(library_root)).await?;
    Ok(())
}

async fn home(State(state): State<AppState>) -> Result<Response, ReaderError> {
    let library = open_library(&state)?;
    let recent = library.recent_revisions(10)?;
    let mut body = String::from(
        "<h1>Your offline encyclopedia</h1>\
         <p>Read and search revisions already captured in this library.</p>",
    );
    body.push_str(&search_form("", None));
    body.push_str("<h2>Recently captured changes</h2>");
    body.push_str(&revision_list(&library, &recent)?);
    Ok(page(StatusCode::OK, "Offline encyclopedia", &body))
}

async fn search(
    State(state): State<AppState>,
    Query(parameters): Query<SearchParameters>,
) -> Result<Response, ReaderError> {
    let wiki_id = optional_wiki_id(parameters.wiki)?;
    let query = parameters.q.as_deref().unwrap_or("").trim();
    let mut body = String::from("<h1>Search</h1>");
    body.push_str(&search_form(query, wiki_id));
    if query.is_empty() {
        body.push_str("<p class=\"meta\">Enter a word or FTS expression.</p>");
        return Ok(page(StatusCode::OK, "Search", &body));
    }

    let library = open_library(&state)?;
    let index = SqliteSearchIndex::open(&library)?;
    let mut search_query = SearchQuery::new(query);
    if let Some(wiki_id) = wiki_id {
        search_query = search_query.for_wiki(wiki_id);
    }
    let hits = index.search(search_query).map_err(ReaderError::search)?;
    body.push_str(&format!(
        "<h2>{} result{}</h2><ol>",
        hits.len(),
        plural(hits.len() as u64)
    ));
    for hit in hits {
        let revision =
            library
                .revision(hit.wiki_id, hit.revision_id)?
                .ok_or(ReaderError::corrupt(
                    "search index points to a missing revision",
                ))?;
        let source = canonical_source(&library, &revision)?;
        let excerpt = excerpt(&to_plain_text(&source), 240);
        body.push_str("<li><h3><a href=\"");
        body.push_str(&escape_attribute(&article_url(&hit.title, hit.wiki_id)));
        body.push_str("\">");
        body.push_str(&escape_html(hit.title.as_str()));
        body.push_str("</a></h3><p>");
        body.push_str(&escape_html(&excerpt));
        body.push_str("</p></li>");
    }
    body.push_str("</ol>");
    Ok(page(StatusCode::OK, "Search", &body))
}

async fn article(
    State(state): State<AppState>,
    RoutePath(raw_title): RoutePath<String>,
    Query(query): Query<WikiQuery>,
) -> Result<Response, ReaderError> {
    let title = PageTitle::new(raw_title.replace('_', " ")).map_err(ReaderError::bad_request)?;
    let wiki_id = optional_wiki_id(query.wiki)?;
    let library = open_library(&state)?;
    let page_data = unique_page(library.pages_by_title(&title, wiki_id)?, &title)?;
    let revision_id = page_data
        .current_revision_id
        .ok_or_else(|| ReaderError::not_found("this page has no captured revision"))?;
    let stored_revision =
        library
            .revision(page_data.wiki_id, revision_id)?
            .ok_or(ReaderError::corrupt(
                "page head points to a missing revision",
            ))?;
    let source = canonical_source(&library, &stored_revision)?;
    let mut body = format!("<h1>{}</h1>", escape_html(page_data.title.as_str()));
    body.push_str(&revision_meta(&page_data, &stored_revision));
    body.push_str("<article>");
    body.push_str(&markdown_to_html(
        &to_markdown(&source),
        page_data.wiki_id,
        &library,
    )?);
    body.push_str("</article><h2>Source and attribution</h2>");
    body.push_str(&source_notice(&page_data, &stored_revision));
    Ok(page(StatusCode::OK, page_data.title.as_str(), &body))
}

async fn history(
    State(state): State<AppState>,
    RoutePath(raw_page_id): RoutePath<u64>,
    Query(query): Query<WikiQuery>,
) -> Result<Response, ReaderError> {
    let page_id = PageId::new(raw_page_id).map_err(ReaderError::bad_request)?;
    let wiki_id = optional_wiki_id(query.wiki)?;
    let library = open_library(&state)?;
    let page_data = if let Some(wiki_id) = wiki_id {
        library
            .page(wiki_id, page_id)?
            .ok_or_else(|| ReaderError::not_found("page was not found"))?
    } else {
        unique_page_by_id(library.pages_by_id(page_id)?, page_id)?
    };
    let revisions = library.revisions_for_page(page_data.wiki_id, page_data.page_id)?;
    let mut body = format!(
        "<h1>History: <a href=\"{}\">{}</a></h1><ol>",
        escape_attribute(&article_url(&page_data.title, page_data.wiki_id)),
        escape_html(page_data.title.as_str())
    );
    for item in revisions {
        let current = if page_data.current_revision_id == Some(item.revision_id) {
            " <strong>current</strong>"
        } else {
            ""
        };
        body.push_str(&format!(
            "<li><a href=\"{}\">Revision {}</a> — <time>{}</time> — {}{}",
            escape_attribute(&revision_url(item.revision_id, page_data.wiki_id)),
            item.revision_id,
            escape_html(&item.timestamp),
            escape_html(item.author.as_deref().unwrap_or("author hidden")),
            current,
        ));
        if let Some(comment) = item
            .comment
            .as_deref()
            .filter(|comment| !comment.is_empty())
        {
            body.push_str("<br><span class=\"meta\">");
            body.push_str(&escape_html(comment));
            body.push_str("</span>");
        }
        body.push_str("</li>");
    }
    body.push_str("</ol>");
    Ok(page(StatusCode::OK, "Revision history", &body))
}

async fn revision(
    State(state): State<AppState>,
    RoutePath(raw_revision_id): RoutePath<u64>,
    Query(query): Query<WikiQuery>,
) -> Result<Response, ReaderError> {
    let revision_id = RevisionId::new(raw_revision_id).map_err(ReaderError::bad_request)?;
    let wiki_id = optional_wiki_id(query.wiki)?;
    let library = open_library(&state)?;
    let (wiki_id, stored_revision) = unique_revision(&library, revision_id, wiki_id)?;
    let page_data = library
        .page(wiki_id, stored_revision.page_id)?
        .ok_or(ReaderError::corrupt("revision points to a missing page"))?;
    let source = canonical_source(&library, &stored_revision)?;
    let mut body = format!(
        "<h1>{} — revision {}</h1>",
        escape_html(page_data.title.as_str()),
        revision_id
    );
    body.push_str(&revision_meta(&page_data, &stored_revision));
    body.push_str("<article>");
    body.push_str(&markdown_to_html(&to_markdown(&source), wiki_id, &library)?);
    body.push_str("</article><h2>Source and attribution</h2>");
    body.push_str(&source_notice(&page_data, &stored_revision));
    Ok(page(StatusCode::OK, "Captured revision", &body))
}

async fn revision_diff(
    State(state): State<AppState>,
    RoutePath((raw_from, raw_to)): RoutePath<(u64, u64)>,
    Query(query): Query<WikiQuery>,
) -> Result<Response, ReaderError> {
    let from_id = RevisionId::new(raw_from).map_err(ReaderError::bad_request)?;
    let to_id = RevisionId::new(raw_to).map_err(ReaderError::bad_request)?;
    let wiki_id = optional_wiki_id(query.wiki)?;
    let library = open_library(&state)?;
    let (from_wiki, from) = unique_revision(&library, from_id, wiki_id)?;
    let (to_wiki, to) = unique_revision(&library, to_id, wiki_id)?;
    if from_wiki != to_wiki || from.page_id != to.page_id {
        return Err(ReaderError::bad_request(
            "diff revisions must belong to the same page and wiki",
        ));
    }
    let older = canonical_source(&library, &from)?;
    let newer = canonical_source(&library, &to)?;
    let comparison = diff(&older, &newer, DiffMode::ExactSource);
    let mut body = format!(
        "<h1>Diff: revision {} → {}</h1><table class=\"diff\"><tbody>",
        from_id, to_id
    );
    for line in comparison.lines {
        let (class, prefix) = match line.tag {
            DiffTag::Equal => ("equal", " "),
            DiffTag::Delete => ("delete", "−"),
            DiffTag::Insert => ("insert", "+"),
        };
        body.push_str(&format!("<tr class=\"{class}\"><td>{prefix}</td><td>"));
        for span in line.spans {
            let escaped = escape_html(span.text.trim_end_matches(['\r', '\n']));
            match span.tag {
                DiffTag::Delete => body.push_str(&format!("<del>{escaped}</del>")),
                DiffTag::Insert => body.push_str(&format!("<ins>{escaped}</ins>")),
                DiffTag::Equal => body.push_str(&escaped),
            }
        }
        body.push_str("</td></tr>");
    }
    body.push_str("</tbody></table>");
    Ok(page(StatusCode::OK, "Revision diff", &body))
}

async fn changes(State(state): State<AppState>) -> Result<Response, ReaderError> {
    let library = open_library(&state)?;
    let recent = library.recent_revisions(100)?;
    let mut body = String::from("<h1>Captured changes</h1>");
    body.push_str(&revision_list(&library, &recent)?);
    Ok(page(StatusCode::OK, "Captured changes", &body))
}

async fn collections(State(state): State<AppState>) -> Result<Response, ReaderError> {
    let library = open_library(&state)?;
    let collections = library.collections()?;
    let mut body = String::from("<h1>Collections</h1><ul>");
    for collection in collections {
        body.push_str(&format!(
            "<li><strong>{}</strong> <span class=\"meta\">wiki {}, {} captured page{}</span></li>",
            escape_html(&collection.name),
            collection.wiki_id,
            collection.page_count,
            plural(collection.page_count),
        ));
    }
    body.push_str("</ul>");
    Ok(page(StatusCode::OK, "Collections", &body))
}

async fn about() -> Response {
    page(
        StatusCode::OK,
        "Source and integrity",
        "<h1>Source and integrity</h1>\
         <p>WikiSyncer stores exact public wikitext and revision metadata returned by \
         the configured MediaWiki source. Reading views are deterministic derived content.</p>\
         <p>Object verification means the captured bytes have not changed locally. It does \
         not prove that an upstream statement is true, unbiased, or still publicly available.</p>\
         <p>This reader uses bundled assets and does not request remote fonts, scripts, styles, \
         or images.</p>",
    )
}

async fn stylesheet() -> Response {
    secured_response(StatusCode::OK, "text/css; charset=utf-8", CSS.to_owned())
}

async fn not_found() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "Not found",
        "<h1>Not found</h1><p>The requested local page does not exist.</p>",
    )
}

fn open_library(state: &AppState) -> Result<Library, ReaderError> {
    Library::open(&state.library_root).map_err(ReaderError::from)
}

fn unique_page(mut matches: Vec<StoredPage>, title: &PageTitle) -> Result<StoredPage, ReaderError> {
    match matches.len() {
        0 => Err(ReaderError::not_found(format!(
            "{title} was not found in the offline library"
        ))),
        1 => Ok(matches.remove(0)),
        count => Err(ReaderError::bad_request(format!(
            "{title} matched {count} wikis; add ?wiki=<id> to select one source"
        ))),
    }
}

fn unique_page_by_id(
    mut matches: Vec<StoredPage>,
    page_id: PageId,
) -> Result<StoredPage, ReaderError> {
    match matches.len() {
        0 => Err(ReaderError::not_found(format!(
            "page {page_id} was not found"
        ))),
        1 => Ok(matches.remove(0)),
        count => Err(ReaderError::bad_request(format!(
            "page {page_id} matched {count} wikis; add ?wiki=<id> to select one source"
        ))),
    }
}

fn unique_revision(
    library: &Library,
    revision_id: RevisionId,
    wiki_id: Option<WikiId>,
) -> Result<(WikiId, StoredRevision), ReaderError> {
    if let Some(wiki_id) = wiki_id {
        return library
            .revision(wiki_id, revision_id)?
            .map(|revision| (wiki_id, revision))
            .ok_or_else(|| {
                ReaderError::not_found(format!("revision {revision_id} was not found"))
            });
    }
    let mut matches = library.revisions_by_id(revision_id)?;
    match matches.len() {
        0 => Err(ReaderError::not_found(format!(
            "revision {revision_id} was not found"
        ))),
        1 => Ok(matches.remove(0)),
        count => Err(ReaderError::bad_request(format!(
            "revision {revision_id} matched {count} wikis; add ?wiki=<id> to select one source"
        ))),
    }
}

fn optional_wiki_id(raw: Option<u64>) -> Result<Option<WikiId>, ReaderError> {
    raw.map(WikiId::new)
        .transpose()
        .map_err(ReaderError::bad_request)
}

fn canonical_source(library: &Library, revision: &StoredRevision) -> Result<String, ReaderError> {
    let bytes = library.read_object(revision.content_object_id)?;
    String::from_utf8(bytes).map_err(|_| ReaderError::corrupt("canonical source is not UTF-8"))
}

fn revision_list(
    library: &Library,
    revisions: &[(WikiId, StoredRevision)],
) -> Result<String, ReaderError> {
    if revisions.is_empty() {
        return Ok("<p class=\"meta\">No revisions have been captured yet.</p>".to_owned());
    }
    let mut output = String::from("<ol>");
    for (wiki_id, revision) in revisions {
        let page_data = library
            .page(*wiki_id, revision.page_id)?
            .ok_or(ReaderError::corrupt("revision points to a missing page"))?;
        output.push_str(&format!(
            "<li><a href=\"{}\">{}</a> — <a href=\"{}\">revision {}</a> — <time>{}</time></li>",
            escape_attribute(&article_url(&page_data.title, *wiki_id)),
            escape_html(page_data.title.as_str()),
            escape_attribute(&revision_url(revision.revision_id, *wiki_id)),
            revision.revision_id,
            escape_html(&revision.timestamp),
        ));
    }
    output.push_str("</ol>");
    Ok(output)
}

fn revision_meta(page_data: &StoredPage, revision: &StoredRevision) -> String {
    format!(
        "<p class=\"meta\">Wiki {} · Page {} · Revision {} · <time>{}</time> · \
         <a href=\"{}\">revision history</a></p>",
        page_data.wiki_id,
        page_data.page_id,
        revision.revision_id,
        escape_html(&revision.timestamp),
        escape_attribute(&format!(
            "/page/{}/history?wiki={}",
            page_data.page_id, page_data.wiki_id
        )),
    )
}

fn source_notice(page_data: &StoredPage, revision: &StoredRevision) -> String {
    format!(
        "<p class=\"notice\">Captured from MediaWiki at revision {} ({}) and stored as \
         <code>{}</code>. Wikipedia text is generally available under CC BY-SA; consult the \
         configured source and page history for complete attribution and license details. \
         Integrity here means verified since local capture, not verified as true.</p>\
         <p><a href=\"{}\">View captured revision metadata</a></p>",
        revision.revision_id,
        escape_html(&revision.timestamp),
        escape_html(&revision.content_object_id.to_string()),
        escape_attribute(&revision_url(revision.revision_id, page_data.wiki_id)),
    )
}

fn search_form(query: &str, wiki_id: Option<WikiId>) -> String {
    let wiki = wiki_id.map_or_else(String::new, |wiki_id| {
        format!("<input type=\"hidden\" name=\"wiki\" value=\"{wiki_id}\">")
    });
    format!(
        "<form action=\"/search\" method=\"get\"><label>Search captured pages \
         <input type=\"search\" name=\"q\" value=\"{}\"></label>{wiki}<button>Search</button></form>",
        escape_attribute(query),
    )
}

fn markdown_to_html(
    markdown: &str,
    wiki_id: WikiId,
    library: &Library,
) -> Result<String, ReaderError> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut events = Vec::new();
    let mut unavailable_links = Vec::new();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Html(value) | Event::InlineHtml(value) => events.push(Event::Text(value)),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let unavailable = if let Some(target) = internal_link_title(&dest_url) {
                    library.pages_by_title(&target, Some(wiki_id))?.is_empty()
                } else {
                    false
                };
                unavailable_links.push(unavailable);
                events.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url: CowStr::from(rewrite_link(&dest_url, wiki_id)),
                    title,
                    id,
                }));
            }
            Event::End(TagEnd::Link) => {
                events.push(Event::End(TagEnd::Link));
                if unavailable_links.pop().unwrap_or(false) {
                    events.push(Event::Text(CowStr::from(" (not captured locally)")));
                }
            }
            Event::Start(Tag::Image {
                link_type,
                title,
                id,
                ..
            }) => events.push(Event::Start(Tag::Image {
                link_type,
                dest_url: CowStr::from(
                    "data:image/gif;base64,R0lGODlhAQABAAD/ACwAAAAAAQABAAACADs=",
                ),
                title,
                id,
            })),
            other => events.push(other),
        }
    }
    let mut output = String::new();
    html::push_html(&mut output, events.into_iter());
    Ok(output)
}

fn rewrite_link(destination: &str, wiki_id: WikiId) -> String {
    if destination.starts_with("https://") || destination.starts_with("http://") {
        return destination.to_owned();
    }
    if destination.starts_with('#') {
        return destination.to_owned();
    }
    if destination.contains("://") || destination.starts_with("mailto:") {
        return "#".to_owned();
    }
    let (_, fragment) = destination
        .split_once('#')
        .map_or((destination, None), |(path, fragment)| {
            (path, Some(fragment))
        });
    let Some(title) = internal_link_title(destination) else {
        return "#".to_owned();
    };
    let mut url = article_url(&title, wiki_id);
    if let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) {
        url.push('#');
        url.push_str(&utf8_percent_encode(fragment, NON_ALPHANUMERIC).to_string());
    }
    url
}

fn internal_link_title(destination: &str) -> Option<PageTitle> {
    if destination.starts_with("https://")
        || destination.starts_with("http://")
        || destination.starts_with('#')
        || destination.contains("://")
        || destination.starts_with("mailto:")
    {
        return None;
    }
    let path = destination
        .split_once('#')
        .map_or(destination, |(path, _)| path);
    let decoded = percent_decode_str(path.trim_start_matches(['.', '/']))
        .decode_utf8_lossy()
        .replace('_', " ");
    PageTitle::new(decoded).ok()
}

fn article_url(title: &PageTitle, wiki_id: WikiId) -> String {
    format!(
        "/wiki/{}?wiki={wiki_id}",
        utf8_percent_encode(title.as_str(), NON_ALPHANUMERIC)
    )
}

fn revision_url(revision_id: RevisionId, wiki_id: WikiId) -> String {
    format!("/revision/{revision_id}?wiki={wiki_id}")
}

fn excerpt(body: &str, maximum: usize) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = collapsed.chars();
    let result = characters.by_ref().take(maximum).collect::<String>();
    if characters.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let document = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><link rel=\"stylesheet\" href=\"/assets/reader.css\"></head>\
         <body><header><nav><a href=\"/\">WikiSyncer</a><a href=\"/search\">Search</a>\
         <a href=\"/changes\">Changes</a><a href=\"/collections\">Collections</a>\
         <a href=\"/about/source-and-integrity\">Source &amp; integrity</a></nav></header>\
         <main>{body}</main></body></html>",
        escape_html(title),
    );
    secured_response(status, "text/html; charset=utf-8", document)
}

fn secured_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_attribute(value: &str) -> String {
    escape_html(value)
}

#[derive(Debug)]
struct ReaderError {
    status: StatusCode,
    message: String,
}

impl ReaderError {
    fn bad_request(error: impl fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn not_found(error: impl fmt::Display) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: error.to_string(),
        }
    }

    fn corrupt(message: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.to_owned(),
        }
    }

    fn search(error: wikisync_search::SearchError) -> Self {
        match error {
            wikisync_search::SearchError::EmptyQuery
            | wikisync_search::SearchError::InvalidLimit(_)
            | wikisync_search::SearchError::Sqlite(_) => Self::bad_request(error),
            _ => Self::corrupt("the local search index could not be read"),
        }
    }
}

impl From<wikisync_store::StoreError> for ReaderError {
    fn from(_: wikisync_store::StoreError) -> Self {
        Self::corrupt("the local archive could not be read")
    }
}

impl From<wikisync_search::SearchError> for ReaderError {
    fn from(_: wikisync_search::SearchError) -> Self {
        Self::corrupt("the local search index could not be opened")
    }
}

impl IntoResponse for ReaderError {
    fn into_response(self) -> Response {
        page(
            self.status,
            self.status.canonical_reason().unwrap_or("Reader error"),
            &format!("<h1>Reader error</h1><p>{}</p>", escape_html(&self.message)),
        )
    }
}

/// Failure to bind or run the loopback reader.
#[derive(Debug)]
pub enum ServeError {
    /// The requested listener was not restricted to this machine.
    NonLoopbackAddress(SocketAddr),
    /// The listener could not be bound or served.
    Io(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopbackAddress(address) => write!(
                formatter,
                "refusing non-loopback reader address {address}; LAN serving is not available"
            ),
            Self::Io(error) => write!(formatter, "local reader I/O error: {error}"),
        }
    }
}

impl Error for ServeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NonLoopbackAddress(_) => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for ServeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;
    use wikisync_content::to_search_content;
    use wikisync_core::{PageId, PageTitle, RevisionId};
    use wikisync_search::{SearchDocument, SearchIndex, SqliteSearchIndex};
    use wikisync_store::{CurrentRevisionCapture, Library, RevisionCapture};

    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        wiki_id: WikiId,
        page_id: PageId,
        current_revision: RevisionId,
        older_revision: RevisionId,
        title: PageTitle,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().to_path_buf();
        let mut library = Library::open(&root).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Languages")
            .expect("collection");
        let page_id = PageId::new(25_357_340).expect("page ID");
        let older_revision = RevisionId::new(1_300_000_000).expect("old revision");
        let current_revision = RevisionId::new(1_300_000_001).expect("current revision");
        let title = PageTitle::new("Rust (programming language)").expect("title");
        let current_source = b"== Rust ==\nA [[systems programming language]] with \
            [https://example.com external docs]. <script>alert('ignored')</script>";
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id,
                    namespace: 0,
                    title: &title,
                    revision_id: current_revision,
                    parent_id: Some(older_revision),
                    timestamp: "2026-08-20T12:00:00Z",
                    author: Some("Fixture editor"),
                    author_id: Some(42),
                    comment: Some("Improve the article"),
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: current_source,
                },
            )
            .expect("current revision");
        library
            .capture_revision(
                wiki_id,
                page_id,
                &RevisionCapture {
                    revision_id: older_revision,
                    parent_id: None,
                    timestamp: "2026-08-19T12:00:00Z",
                    author: Some("Earlier editor"),
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"== Rust ==\nA programming language.",
                },
            )
            .expect("older revision");
        let current_source = std::str::from_utf8(current_source).expect("fixture source");
        let content = to_search_content(current_source);
        let mut index = SqliteSearchIndex::open(&library).expect("search index");
        index
            .index_document(&SearchDocument {
                wiki_id,
                page_id,
                revision_id: current_revision,
                title: &title,
                aliases: "Rust language",
                headings: &content.headings,
                body: &content.body,
                categories: "Programming languages",
                captions: "",
                transformer_version: content.transformer_version.as_str(),
            })
            .expect("index document");
        drop(index);
        drop(library);
        Fixture {
            _directory: directory,
            root,
            wiki_id,
            page_id,
            current_revision,
            older_revision,
            title,
        }
    }

    async fn response_text(
        application: Router,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = application
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body");
        (
            status,
            headers,
            String::from_utf8(body.to_vec()).expect("UTF-8 response"),
        )
    }

    #[tokio::test]
    async fn reader_supports_the_offline_navigation_routes() {
        let fixture = fixture();
        let title = utf8_percent_encode(fixture.title.as_str(), NON_ALPHANUMERIC);
        let routes = [
            "/".to_owned(),
            "/search?q=systems".to_owned(),
            format!("/wiki/{title}?wiki={}", fixture.wiki_id),
            format!("/page/{}/history?wiki={}", fixture.page_id, fixture.wiki_id),
            format!(
                "/revision/{}?wiki={}",
                fixture.current_revision, fixture.wiki_id
            ),
            format!(
                "/diff/{}/{}?wiki={}",
                fixture.older_revision, fixture.current_revision, fixture.wiki_id
            ),
            "/changes".to_owned(),
            "/collections".to_owned(),
            "/about/source-and-integrity".to_owned(),
        ];
        for route in routes {
            let (status, headers, body) = response_text(router(&fixture.root), &route).await;
            assert_eq!(status, StatusCode::OK, "route {route}: {body}");
            assert_eq!(
                headers
                    .get(CONTENT_SECURITY_POLICY)
                    .and_then(|value| value.to_str().ok()),
                Some(CSP)
            );
        }
    }

    #[tokio::test]
    async fn rendered_article_sanitizes_html_and_rewrites_internal_links() {
        let fixture = fixture();
        let title = utf8_percent_encode(fixture.title.as_str(), NON_ALPHANUMERIC);
        let (_, _, body) = response_text(
            router(&fixture.root),
            &format!("/wiki/{title}?wiki={}", fixture.wiki_id),
        )
        .await;
        assert!(body.contains("/wiki/systems%20programming%20language?wiki=1"));
        assert!(body.contains("not captured locally"));
        assert!(body.contains("href=\"https://example.com\""));
        assert!(!body.contains("<script"));
        assert!(!body.contains("<img src=\"http"));
    }

    #[tokio::test]
    async fn offline_crawl_has_no_outbound_resource_requests() {
        let fixture = fixture();
        let mut pending = vec!["/".to_owned()];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(uri) = pending.pop() {
            if !visited.insert(uri.clone()) {
                continue;
            }
            let (status, _, body) = response_text(router(&fixture.root), &uri).await;
            assert!(
                status.is_success() || status == StatusCode::NOT_FOUND,
                "crawl failed for {uri}: {status}"
            );
            for resource in attribute_values(&body, "src")
                .into_iter()
                .chain(stylesheet_links(&body))
            {
                assert!(
                    resource.starts_with('/') || resource.starts_with("data:"),
                    "outbound resource URL in {uri}: {resource}"
                );
            }
            for link in attribute_values(&body, "href") {
                if link.starts_with('/') {
                    pending.push(link.replace("&amp;", "&"));
                }
            }
        }
        assert!(visited.contains("/assets/reader.css"));
        assert!(visited.iter().any(|path| path.starts_with("/wiki/")));
        assert!(visited.iter().any(|path| path.starts_with("/revision/")));
    }

    #[tokio::test]
    async fn non_loopback_listener_is_rejected_before_binding() {
        let fixture = fixture();
        let address = "0.0.0.0:8080".parse().expect("socket address");
        assert!(matches!(
            serve(&fixture.root, address).await,
            Err(ServeError::NonLoopbackAddress(value)) if value == address
        ));
    }

    fn attribute_values(document: &str, attribute: &str) -> Vec<String> {
        let prefix = format!("{attribute}=\"");
        document
            .split(&prefix)
            .skip(1)
            .filter_map(|remaining| remaining.split_once('"').map(|(value, _)| value.to_owned()))
            .collect()
    }

    fn stylesheet_links(document: &str) -> Vec<String> {
        document
            .split("<link ")
            .skip(1)
            .filter_map(|tag| tag.split_once('>').map(|(tag, _)| tag))
            .filter(|tag| tag.contains("rel=\"stylesheet\""))
            .flat_map(|tag| attribute_values(tag, "href"))
            .collect()
    }
}
