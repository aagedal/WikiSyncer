//! Loopback-only routes, templates, and offline assets for the local reader.

use std::error::Error;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
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
use wikisync_content::{
    DiffMode, DiffTag, ThumbnailLimits, diff, to_markdown, to_plain_text, validate_thumbnail,
};
use wikisync_core::{PageId, PageTitle, RevisionId, WikiId};
use wikisync_search::{SearchIndex, SearchQuery, SqliteSearchIndex};
use wikisync_store::{Library, StoredPage, StoredRevision, StoredRevisionMedia};

const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self' data:; \
    script-src 'none'; connect-src 'none'; font-src 'none'; form-action 'self'; \
    base-uri 'none'; frame-ancestors 'none'";
const CSS: &str = r#"
:root {
  color-scheme: light dark;
  --canvas: #f7f5ef;
  --surface: #fffefa;
  --surface-muted: #efede6;
  --text: #24221f;
  --muted: #69645d;
  --border: #d6d1c6;
  --accent: #155f86;
  --accent-strong: #0d4564;
  --notice: #9c6500;
  --insert: #dff4e4;
  --delete: #f8dfdf;
  font-family: Charter, "Bitstream Charter", "Iowan Old Style", Georgia, serif;
  font-size: 18px;
  line-height: 1.68;
  text-rendering: optimizeLegibility;
}
* { box-sizing: border-box; }
html { scroll-padding-top: 6rem; }
body { background: var(--canvas); color: var(--text); margin: 0; }
a { color: var(--accent); text-decoration-thickness: .08em; text-underline-offset: .15em; }
a:hover { color: var(--accent-strong); text-decoration-thickness: .12em; }
a:focus-visible, button:focus-visible, input:focus-visible, [tabindex="0"]:focus-visible {
  outline: .18rem solid var(--accent);
  outline-offset: .18rem;
}
.skip-link { background: var(--surface); left: 1rem; padding: .5rem .75rem; position: fixed; top: -5rem; z-index: 3; }
.skip-link:focus { top: 1rem; }
.site-header { background: color-mix(in srgb, var(--surface) 94%, transparent); border-bottom: 1px solid var(--border); }
.site-header-inner { align-items: center; display: flex; gap: 2rem; justify-content: space-between; margin: 0 auto; max-width: 76rem; padding: .8rem 1.25rem; }
.brand { color: var(--text); font-family: ui-sans-serif, system-ui, sans-serif; font-size: 1.05rem; font-weight: 750; text-decoration: none; }
.brand small { color: var(--muted); display: block; font-size: .68rem; font-weight: 500; letter-spacing: .08em; text-transform: uppercase; }
.primary-nav { display: flex; flex-wrap: wrap; font-family: ui-sans-serif, system-ui, sans-serif; font-size: .82rem; gap: .3rem .9rem; }
.primary-nav a { white-space: nowrap; }
main { margin: 0 auto; max-width: 76rem; min-height: 70vh; padding: clamp(1.5rem, 4vw, 3.5rem) 1.25rem 4rem; }
main > :not(article):not(.wide) { max-width: 58rem; }
h1, h2, h3, h4, h5, h6 { line-height: 1.18; margin: 1.8em 0 .55em; text-wrap: balance; }
h1 { font-size: clamp(2rem, 7vw, 3.25rem); letter-spacing: -.035em; margin-top: 0; }
h2 { border-bottom: 1px solid var(--border); font-size: clamp(1.45rem, 4vw, 2rem); padding-bottom: .2em; }
h3 { font-size: 1.3rem; }
p, li { max-width: 72ch; }
li + li { margin-top: .32em; }
article { font-size: 1.04rem; max-width: 72ch; overflow-wrap: anywhere; }
article > :first-child { margin-top: 0; }
article blockquote { border-left: .22rem solid var(--border); color: var(--muted); margin-left: 0; padding-left: 1.2rem; }
pre, code { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: .88em; }
code { background: var(--surface-muted); border-radius: .18rem; padding: .08em .25em; }
pre { background: var(--surface-muted); border: 1px solid var(--border); border-radius: .3rem; overflow-x: auto; padding: 1rem; }
pre code { background: none; padding: 0; }
form { align-items: end; display: flex; flex-wrap: wrap; gap: .65rem; margin: 1.2rem 0 2rem; }
label { font-family: ui-sans-serif, system-ui, sans-serif; font-size: .86rem; font-weight: 650; }
input { background: var(--surface); border: 1px solid var(--border); border-radius: .25rem; color: var(--text); display: block; font: inherit; margin-top: .25rem; min-width: min(30rem, 82vw); padding: .5rem .65rem; }
button { background: var(--accent); border: 0; border-radius: .25rem; color: white; cursor: pointer; font: 650 .9rem ui-sans-serif, system-ui, sans-serif; padding: .55rem .9rem; }
.meta, time { color: var(--muted); font-family: ui-sans-serif, system-ui, sans-serif; font-size: .82rem; }
.notice { background: var(--surface); border: 1px solid var(--border); border-left: .3rem solid var(--notice); border-radius: .2rem; padding: .9rem 1rem; }
.context-nav { align-items: center; display: flex; flex-wrap: wrap; font: .8rem ui-sans-serif, system-ui, sans-serif; gap: .45rem 1rem; margin: 0 0 1.25rem; }
.context-nav a[aria-current="page"] { color: var(--text); font-weight: 700; text-decoration: none; }
.revision-pager { background: var(--surface); border: 1px solid var(--border); border-radius: .25rem; display: flex; flex-wrap: wrap; font: .8rem ui-sans-serif, system-ui, sans-serif; gap: .4rem 1.2rem; margin: 1rem 0 1.5rem; padding: .6rem .8rem; }
.history-list li, .revision-list li { padding: .22rem 0; }
.provenance { border-top: 1px solid var(--border); margin-top: 3.5rem; max-width: 72ch; padding-top: .5rem; }
.provenance dl { display: grid; font: .82rem ui-sans-serif, system-ui, sans-serif; gap: .25rem 1rem; grid-template-columns: max-content minmax(0, 1fr); margin: 1rem 0; }
.provenance dt { color: var(--muted); font-weight: 650; }
.provenance dd { margin: 0; overflow-wrap: anywhere; }
.table-scroll { margin: 1.5rem 0; max-width: 100%; overflow-x: auto; }
.table-scroll table { border-collapse: collapse; font: .9rem ui-sans-serif, system-ui, sans-serif; width: 100%; }
.table-scroll th, .table-scroll td { border: 1px solid var(--border); min-width: 8rem; padding: .5rem .65rem; text-align: left; vertical-align: top; }
.table-scroll th { background: var(--surface-muted); font-weight: 700; }
.table-scroll tbody tr:nth-child(even) { background: color-mix(in srgb, var(--surface-muted) 55%, transparent); }
.footnote-reference { font-family: ui-sans-serif, system-ui, sans-serif; font-size: .72em; line-height: 1; }
.footnote-definition { border-top: 1px solid var(--border); color: var(--muted); font-size: .86rem; padding: .55rem 0 .1rem 2rem; position: relative; }
.footnote-definition-label { font-family: ui-sans-serif, system-ui, sans-serif; font-weight: 700; left: .25rem; position: absolute; }
.footnote-definition p { margin: 0; }
.site-footer { border-top: 1px solid var(--border); color: var(--muted); font: .78rem ui-sans-serif, system-ui, sans-serif; margin: 0 auto; max-width: 76rem; padding: 1rem 1.25rem 2rem; }
.diff { border-collapse: collapse; font-family: ui-monospace, monospace; font-size: .82rem; width: 100%; }
.diff td { border: 1px solid var(--border); padding: .2rem .5rem; vertical-align: top; white-space: pre-wrap; }
.diff .delete { background: var(--delete); }
.diff .insert { background: var(--insert); }
.diff del { background: color-mix(in srgb, #c22 25%, transparent); }
.diff ins { background: color-mix(in srgb, #198a36 25%, transparent); }
.captured-media { display: grid; gap: 1.5rem; margin: 2rem 0; }
.captured-media figure { background: var(--surface); border: 1px solid var(--border); border-radius: .3rem; margin: 0; overflow: hidden; }
.captured-media img { display: block; height: auto; max-height: 42rem; max-width: 100%; object-fit: contain; }
.captured-media figcaption { border-top: 1px solid var(--border); font: .82rem ui-sans-serif, system-ui, sans-serif; padding: .7rem .85rem; }
.captured-media figcaption p { margin: .25rem 0; }
@media (max-width: 44rem) {
  :root { font-size: 16px; }
  .site-header-inner { align-items: flex-start; flex-direction: column; gap: .65rem; }
  .primary-nav { gap: .3rem .75rem; }
  main { padding-top: 2rem; }
  .provenance dl { grid-template-columns: 1fr; }
  .provenance dd + dt { margin-top: .4rem; }
}
@media (prefers-color-scheme: dark) {
  :root { --canvas: #171817; --surface: #202220; --surface-muted: #292c29; --text: #ecebe5; --muted: #b0ada4; --border: #41443f; --accent: #7fc9ef; --accent-strong: #b6e4fa; --notice: #e4a83a; --insert: #153c22; --delete: #482020; }
  button { color: #10222c; }
}
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
        .route(
            "/media/{wiki_id}/{revision_id}/{placement_index}",
            get(media_bytes),
        )
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

/// Starts a ready-to-accept reader on an ephemeral IPv4 loopback port.
///
/// Binding completes before this function returns, so GUI callers can immediately
/// open [`ReaderHandle::local_url`]. Dropping the handle requests graceful shutdown;
/// callers that need confirmation should await [`ReaderHandle::shutdown`].
pub async fn start_loopback(library_root: impl AsRef<Path>) -> Result<ReaderHandle, ServeError> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let local_url = format!("http://{address}/");
    let application = router(library_root.as_ref());
    let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
    });
    Ok(ReaderHandle {
        address,
        local_url,
        shutdown_sender: Some(shutdown_sender),
        task: Some(task),
    })
}

/// A running ephemeral loopback reader suitable for ownership by the GUI.
#[derive(Debug)]
pub struct ReaderHandle {
    address: SocketAddr,
    local_url: String,
    shutdown_sender: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl ReaderHandle {
    /// Returns the bound loopback socket address, including its allocated port.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns the absolute loopback URL for the reader home page.
    #[must_use]
    pub fn local_url(&self) -> &str {
        &self.local_url
    }

    /// Requests graceful shutdown and waits until the listener has stopped.
    pub async fn shutdown(mut self) -> Result<(), ServeError> {
        self.request_shutdown();
        let task = self
            .task
            .take()
            .expect("reader task is present until shutdown consumes the handle");
        task.await
            .map_err(|error| ServeError::Io(io::Error::other(error)))??;
        Ok(())
    }

    fn request_shutdown(&mut self) {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        self.request_shutdown();
    }
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
    let mut body = page_navigation(&page_data, PageView::Article);
    body.push_str(&format!(
        "<h1>{}</h1>",
        escape_html(page_data.title.as_str())
    ));
    body.push_str(&revision_meta(&page_data, &stored_revision));
    body.push_str("<article>");
    body.push_str(&markdown_to_html(
        &to_markdown(&source),
        page_data.wiki_id,
        &library,
    )?);
    body.push_str("</article>");
    body.push_str(&media_figures(
        &library,
        page_data.wiki_id,
        stored_revision.revision_id,
    )?);
    body.push_str(&source_section(&page_data, &stored_revision));
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
    let mut body = page_navigation(&page_data, PageView::History);
    body.push_str(&format!(
        "<h1>History: <a href=\"{}\">{}</a></h1><ol class=\"history-list\">",
        escape_attribute(&article_url(&page_data.title, page_data.wiki_id)),
        escape_html(page_data.title.as_str())
    ));
    for (index, item) in revisions.iter().enumerate() {
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
        if let Some(older) = revisions.get(index + 1) {
            body.push_str("<br><a class=\"meta\" href=\"");
            body.push_str(&escape_attribute(&diff_url(
                older.revision_id,
                item.revision_id,
                page_data.wiki_id,
            )));
            body.push_str("\">Compare with previous captured revision</a>");
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
    let mut body = page_navigation(&page_data, PageView::Neither);
    body.push_str(&format!(
        "<h1>{} — revision {}</h1>",
        escape_html(page_data.title.as_str()),
        revision_id
    ));
    body.push_str(&revision_meta(&page_data, &stored_revision));
    body.push_str(&revision_pager(&library, &page_data, &stored_revision)?);
    body.push_str("<article>");
    body.push_str(&markdown_to_html(&to_markdown(&source), wiki_id, &library)?);
    body.push_str("</article>");
    body.push_str(&media_figures(
        &library,
        wiki_id,
        stored_revision.revision_id,
    )?);
    body.push_str(&source_section(&page_data, &stored_revision));
    Ok(page(StatusCode::OK, "Captured revision", &body))
}

async fn media_bytes(
    State(state): State<AppState>,
    RoutePath((raw_wiki_id, raw_revision_id, placement_index)): RoutePath<(u64, u64, u32)>,
) -> Result<Response, ReaderError> {
    let wiki_id = WikiId::new(raw_wiki_id).map_err(ReaderError::bad_request)?;
    let revision_id = RevisionId::new(raw_revision_id).map_err(ReaderError::bad_request)?;
    let library = open_library(&state)?;
    let media = library
        .revision_media(wiki_id, revision_id)?
        .into_iter()
        .find(|media| media.placement_index == placement_index)
        .ok_or_else(|| ReaderError::not_found("captured media was not found"))?;
    let bytes = verified_media_bytes(&library, &media)?;
    Ok(secured_binary_response(
        StatusCode::OK,
        media.mime_type.as_str(),
        bytes,
    ))
}

fn verified_media_bytes(
    library: &Library,
    media: &StoredRevisionMedia,
) -> Result<Vec<u8>, ReaderError> {
    let bytes = library.read_object(media.content_object_id)?;
    let pixels = u64::from(media.width)
        .checked_mul(u64::from(media.height))
        .ok_or_else(|| ReaderError::corrupt("captured media dimensions overflowed"))?;
    let limits = ThumbnailLimits {
        max_encoded_bytes: u64::try_from(bytes.len())
            .map_err(|_| ReaderError::corrupt("captured media length overflowed"))?,
        max_width: media.width,
        max_height: media.height,
        max_pixels: pixels,
        max_decoded_bytes: pixels
            .checked_mul(8)
            .ok_or_else(|| ReaderError::corrupt("captured media allocation overflowed"))?,
    };
    let validated = validate_thumbnail(&bytes, media.mime_type.as_str(), &limits)
        .map_err(|_| ReaderError::corrupt("captured media failed passive-raster validation"))?;
    if validated.width != media.width || validated.height != media.height {
        return Err(ReaderError::corrupt(
            "captured media dimensions disagree with its metadata",
        ));
    }
    Ok(bytes)
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
    let page_data = library
        .page(from_wiki, from.page_id)?
        .ok_or(ReaderError::corrupt("revision points to a missing page"))?;
    let mut body = page_navigation(&page_data, PageView::Neither);
    body.push_str(&format!(
        "<h1>Diff: revision {} → {}</h1>\
         <p class=\"meta\">Exact captured wikitext comparison</p>\
         <div class=\"wide table-scroll\" tabindex=\"0\" role=\"region\" \
         aria-label=\"Revision diff\"><table class=\"diff\"><tbody>",
        from_id, to_id
    ));
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
    body.push_str("</tbody></table></div>");
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
         <p>WikiSyncer stores exact public wikitext and revision metadata captured from each \
         configured MediaWiki source. Article HTML is derived locally and can be rebuilt from \
         that canonical record.</p>\
         <p>When an article or revision opens successfully, WikiSyncer has checked the content \
         object bytes used for that view against their local object ID. That check can detect \
         alteration or corruption of those bytes. It is not a claim that a statement is true, \
         unbiased, complete, or still publicly available upstream, and it is not by itself a \
         full-library or manifest-chain verification.</p>\
         <p>Licensing and attribution requirements depend on the configured source. Keep the \
         displayed revision and authorship details when reusing captured material, and consult \
         the source terms. Material retained here may remain available after it is deleted or \
         suppressed upstream, which can carry privacy, copyright, or safety obligations.</p>\
         <p>This reader uses bundled assets and does not request remote fonts, scripts, styles, \
         or images. Article text may retain clearly visible links to external sources.</p>",
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
    let mut output = String::from("<ol class=\"revision-list\">");
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageView {
    Article,
    History,
    Neither,
}

fn page_navigation(page_data: &StoredPage, active: PageView) -> String {
    let article_current = if active == PageView::Article {
        " aria-current=\"page\""
    } else {
        ""
    };
    let history_current = if active == PageView::History {
        " aria-current=\"page\""
    } else {
        ""
    };
    let revision = page_data
        .current_revision_id
        .map_or_else(String::new, |revision_id| {
            format!(
                "<a href=\"{}\">Latest captured revision</a>",
                escape_attribute(&revision_url(revision_id, page_data.wiki_id))
            )
        });
    format!(
        "<nav class=\"context-nav\" aria-label=\"Article\">\
         <a{article_current} href=\"{}\">Article</a>\
         <a{history_current} href=\"/page/{}/history?wiki={}\">History</a>{revision}</nav>",
        escape_attribute(&article_url(&page_data.title, page_data.wiki_id)),
        page_data.page_id,
        page_data.wiki_id,
    )
}

fn revision_pager(
    library: &Library,
    page_data: &StoredPage,
    revision: &StoredRevision,
) -> Result<String, ReaderError> {
    let revisions = library.revisions_for_page(page_data.wiki_id, page_data.page_id)?;
    let Some(position) = revisions
        .iter()
        .position(|candidate| candidate.revision_id == revision.revision_id)
    else {
        return Err(ReaderError::corrupt(
            "revision is missing from its page history",
        ));
    };
    let mut output = String::from("<nav class=\"revision-pager\" aria-label=\"Revision\">");
    if let Some(newer) = position
        .checked_sub(1)
        .and_then(|index| revisions.get(index))
    {
        output.push_str(&format!(
            "<a href=\"{}\">← Newer captured revision</a>",
            escape_attribute(&revision_url(newer.revision_id, page_data.wiki_id))
        ));
    }
    if let Some(older) = revisions.get(position + 1) {
        output.push_str(&format!(
            "<a href=\"{}\">Older captured revision →</a>",
            escape_attribute(&revision_url(older.revision_id, page_data.wiki_id))
        ));
    }
    if let Some(parent_id) = revision.parent_id
        && library
            .revision(page_data.wiki_id, parent_id)?
            .is_some_and(|parent| parent.page_id == page_data.page_id)
    {
        output.push_str(&format!(
            "<a href=\"{}\">Compare with captured parent</a>",
            escape_attribute(&diff_url(
                parent_id,
                revision.revision_id,
                page_data.wiki_id,
            ))
        ));
    }
    output.push_str("</nav>");
    Ok(output)
}

fn source_section(page_data: &StoredPage, revision: &StoredRevision) -> String {
    let author = revision
        .author
        .as_deref()
        .unwrap_or("not publicly recorded");
    format!(
        "<section class=\"provenance\" aria-labelledby=\"source-attribution\">\
         <h2 id=\"source-attribution\">Source, attribution, and integrity</h2>\
         <dl><dt>Configured source</dt><dd>MediaWiki source {}</dd>\
         <dt>Page and revision</dt><dd>Page {} · Revision {}</dd>\
         <dt>Source timestamp</dt><dd>{}</dd><dt>Recorded author</dt><dd>{}</dd>\
         <dt>Captured locally</dt><dd>Unix timestamp {}</dd>\
         <dt>Content object</dt><dd><code>{}</code></dd></dl>\
         <p class=\"notice\">This reading view was derived locally from the exact public \
         wikitext captured for this revision. The content object bytes were checked against \
         the displayed object ID while loading this page. This can detect local alteration or \
         corruption of those bytes; it does not verify that the content is true, unbiased, \
         complete, or still public upstream.</p>\
         <p>Licensing and attribution requirements depend on the configured source. Retain \
         these revision and authorship details and consult the source terms when reusing this \
         material. <a href=\"{}\">Browse the captured revision history</a>.</p></section>",
        page_data.wiki_id,
        page_data.page_id,
        revision.revision_id,
        escape_html(&revision.timestamp),
        escape_html(author),
        revision.captured_at,
        escape_html(&revision.content_object_id.to_string()),
        escape_attribute(&format!(
            "/page/{}/history?wiki={}",
            page_data.page_id, page_data.wiki_id
        )),
    )
}

fn media_figures(
    library: &Library,
    wiki_id: WikiId,
    revision_id: RevisionId,
) -> Result<String, ReaderError> {
    let media = library.revision_media(wiki_id, revision_id)?;
    if media.is_empty() {
        return Ok(String::new());
    }
    let mut output = String::from(
        "<section class=\"captured-media\" aria-labelledby=\"captured-media-heading\">\
         <h2 id=\"captured-media-heading\">Captured media</h2>",
    );
    for item in &media {
        output.push_str(&media_figure(item));
    }
    output.push_str("</section>");
    Ok(output)
}

fn media_figure(media: &StoredRevisionMedia) -> String {
    let caption = media
        .caption
        .as_deref()
        .or(media.alt_text.as_deref())
        .unwrap_or_else(|| media.file_title.as_str());
    let alternative = media
        .alt_text
        .as_deref()
        .or(media.caption.as_deref())
        .unwrap_or_else(|| media.file_title.as_str());
    let source = safe_metadata_link("Source description", &media.description_url);
    let rendition = safe_metadata_link("Observed rendition", &media.original_url);
    let license = media.license_url.as_deref().map_or_else(
        || escape_html(&media.license_name),
        |url| safe_metadata_link(&media.license_name, url),
    );
    format!(
        "<figure><img src=\"/media/{}/{}/{}\" alt=\"{}\" width=\"{}\" height=\"{}\" \
         loading=\"lazy\" decoding=\"async\"><figcaption><p>{}</p>\
         <p><strong>Artist/creator:</strong> {} · <strong>Credit:</strong> {}</p>\
         <p>{source} · <strong>License:</strong> {license} · {} × {} px</p>\
         <p class=\"meta\">{rendition}. Captured locally at Unix timestamp {} as the recorded {} \
         placement. Upstream file hash: <code>{}</code>. Local content object: \
         <code>{}</code>.</p></figcaption></figure>",
        media.wiki_id,
        media.revision_id,
        media.placement_index,
        escape_attribute(alternative),
        media.width,
        media.height,
        escape_html(caption),
        escape_html(&media.author),
        escape_html(&media.attribution),
        media.width,
        media.height,
        media.captured_at,
        escape_html(media.placement_kind.as_str()),
        escape_html(&media.source_sha1),
        escape_html(&media.content_object_id.to_string()),
    )
}

fn safe_metadata_link(label: &str, url: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        format!(
            "<a href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
            escape_attribute(url),
            escape_html(label)
        )
    } else {
        format!("{}: {}", escape_html(label), escape_html(url))
    }
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
    let markdown = reader_markdown(markdown);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES;
    let mut events = Vec::new();
    let mut unavailable_links = Vec::new();
    for event in Parser::new_ext(&markdown, options) {
        match event {
            Event::Html(value) | Event::InlineHtml(value) => events.push(Event::Text(value)),
            event @ Event::Start(Tag::Table(_)) => {
                events.push(Event::Html(CowStr::Borrowed(
                    "<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" \
                     aria-label=\"Article table\">",
                )));
                events.push(event);
            }
            event @ Event::End(TagEnd::Table) => {
                events.push(event);
                events.push(Event::Html(CowStr::Borrowed("</div>")));
            }
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

fn reader_markdown(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut references = Vec::new();
    let mut remaining = markdown;
    while let Some(start) = remaining.find("[ref: ") {
        output.push_str(&remaining[..start]);
        let reference = &remaining[start + 6..];
        let Some(end) = closing_reference_bracket(reference) else {
            output.push_str(&remaining[start..]);
            remaining = "";
            break;
        };
        references.push(reference[..end].trim().to_owned());
        output.push_str(&format!("[^reader-reference-{}]", references.len()));
        remaining = &reference[end + 1..];
    }
    output.push_str(remaining);
    if references.is_empty() {
        return output;
    }
    if !has_reference_heading(&output) {
        output.push_str("\n\n## References\n");
    }
    for (index, reference) in references.iter().enumerate() {
        output.push_str(&format!(
            "\n[^reader-reference-{}]: {}\n",
            index + 1,
            reference
        ));
    }
    output
}

fn closing_reference_bracket(reference: &str) -> Option<usize> {
    let mut depth = 1_u32;
    let mut escaped = false;
    for (offset, character) in reference.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' => depth = depth.saturating_add(1),
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn has_reference_heading(markdown: &str) -> bool {
    markdown.lines().any(|line| {
        let line = line.trim_start();
        if !line.starts_with('#') {
            return false;
        }
        let heading = line.trim_start_matches('#').trim();
        matches!(
            heading.to_ascii_lowercase().as_str(),
            "references" | "notes" | "notes and references"
        )
    })
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

fn diff_url(from_id: RevisionId, to_id: RevisionId, wiki_id: WikiId) -> String {
    format!("/diff/{from_id}/{to_id}?wiki={wiki_id}")
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
         <body><a class=\"skip-link\" href=\"#content\">Skip to content</a>\
         <header class=\"site-header\"><div class=\"site-header-inner\">\
         <a class=\"brand\" href=\"/\">WikiSyncer<small>Offline library</small></a>\
         <nav class=\"primary-nav\" aria-label=\"Library\"><a href=\"/search\">Search</a>\
         <a href=\"/changes\">Changes</a><a href=\"/collections\">Collections</a>\
         <a href=\"/about/source-and-integrity\">Source &amp; integrity</a></nav></div></header>\
         <main id=\"content\">{body}</main><footer class=\"site-footer\">Read-only local archive · \
         Reader styles and other page assets are bundled for offline use.</footer></body></html>",
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

fn secured_binary_response(
    status: StatusCode,
    content_type: &'static str,
    body: Vec<u8>,
) -> Response {
    let mut response = (status, Body::from(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
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
    use wikisync_core::{MediaId, PageId, PageTitle, RevisionId, ThumbnailPolicy};
    use wikisync_search::{SearchDocument, SearchIndex, SqliteSearchIndex};
    use wikisync_store::{
        CurrentRevisionCapture, Library, MediaPlacementKind, RevisionCapture,
        RevisionMediaPlacement, ThumbnailCapture, ThumbnailMimeType,
    };

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

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

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
            [https://example.com external docs].<ref name=\"guide\">See the \
            [https://example.com/guide reference guide].</ref> \
            <script>alert('ignored')</script>\n\n\
            {| class=\"wikitable\"\n\
            ! Channel !! Purpose\n\
            |-\n\
            | Stable || Production use\n\
            |}";
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
        let file_title = PageTitle::new("File:Offline fixture.png").expect("file title");
        library
            .capture_revision_thumbnail(
                wiki_id,
                page_id,
                current_revision,
                ThumbnailPolicy::new(640, 8, 1024).expect("thumbnail policy"),
                &ThumbnailCapture {
                    media_id: MediaId::new(9001).expect("media ID"),
                    file_title: &file_title,
                    source_sha1: "abcdef0123456789abcdef0123456789",
                    original_url: "https://upload.wikimedia.org/offline-fixture.png",
                    description_url: "https://commons.wikimedia.org/wiki/File:Offline_fixture.png",
                    author: "Fixture photographer",
                    attribution: "Fixture photographer / Wikimedia Commons",
                    license_name: "CC BY-SA 4.0",
                    license_url: Some("https://creativecommons.org/licenses/by-sa/4.0/"),
                    width: 1,
                    height: 1,
                    mime_type: ThumbnailMimeType::Png,
                    captured_at: 1_776_000_000,
                    source: VALID_PNG,
                },
                RevisionMediaPlacement {
                    index: 0,
                    kind: MediaPlacementKind::Lead,
                    caption: Some("Offline fixture caption"),
                    alt_text: None,
                },
            )
            .expect("capture thumbnail");
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
        let (status, headers, body) = response_bytes(application, uri).await;
        (
            status,
            headers,
            String::from_utf8(body).expect("UTF-8 response"),
        )
    }

    async fn response_bytes(
        application: Router,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
        (status, headers, body.to_vec())
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
    async fn article_renders_references_tables_and_context_navigation() {
        let fixture = fixture();
        let title = utf8_percent_encode(fixture.title.as_str(), NON_ALPHANUMERIC);
        let (_, _, body) = response_text(
            router(&fixture.root),
            &format!("/wiki/{title}?wiki={}", fixture.wiki_id),
        )
        .await;

        assert!(body.contains("class=\"context-nav\" aria-label=\"Article\""));
        assert!(body.contains("aria-current=\"page\""));
        assert!(body.contains(&format!(
            "/page/{}/history?wiki={}",
            fixture.page_id, fixture.wiki_id
        )));
        assert!(body.contains("class=\"table-scroll\""));
        assert!(body.contains("aria-label=\"Article table\""));
        assert!(body.contains("<th>Channel</th>"));
        assert!(body.contains("class=\"footnote-reference\""));
        assert!(body.contains("<h2>References</h2>"));
        assert!(body.contains("reference guide"));
    }

    #[tokio::test]
    async fn article_and_revision_render_attributed_verified_local_thumbnail() {
        let fixture = fixture();
        let title = utf8_percent_encode(fixture.title.as_str(), NON_ALPHANUMERIC);
        let (_, headers, article) = response_text(
            router(&fixture.root),
            &format!("/wiki/{title}?wiki={}", fixture.wiki_id),
        )
        .await;
        let media_url = format!("/media/{}/{}/0", fixture.wiki_id, fixture.current_revision);
        assert_eq!(
            headers
                .get(CONTENT_SECURITY_POLICY)
                .and_then(|value| value.to_str().ok()),
            Some(CSP)
        );
        assert!(article.contains(&format!("src=\"{media_url}\"")));
        assert!(article.contains("alt=\"Offline fixture caption\""));
        assert!(article.contains("Fixture photographer / Wikimedia Commons"));
        assert!(article.contains("Source description"));
        assert!(article.contains("CC BY-SA 4.0"));
        assert!(article.contains("1 × 1 px"));
        assert!(article.contains("Upstream file hash"));
        assert!(!article.contains("src=\"https://"));

        let (_, _, revision) = response_text(
            router(&fixture.root),
            &format!(
                "/revision/{}?wiki={}",
                fixture.current_revision, fixture.wiki_id
            ),
        )
        .await;
        assert!(revision.contains(&format!("src=\"{media_url}\"")));

        let (status, headers, bytes) = response_bytes(router(&fixture.root), &media_url).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "image/png");
        assert_eq!(headers.get(X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(bytes, VALID_PNG);

        let (missing, _, _) = response_bytes(
            router(&fixture.root),
            &format!("/media/{}/{}/1", fixture.wiki_id, fixture.current_revision),
        )
        .await;
        assert_eq!(missing, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn revision_views_link_adjacent_captures_parent_diff_and_article() {
        let fixture = fixture();
        let (_, _, current) = response_text(
            router(&fixture.root),
            &format!(
                "/revision/{}?wiki={}",
                fixture.current_revision, fixture.wiki_id
            ),
        )
        .await;
        assert!(current.contains("Older captured revision"));
        assert!(current.contains("Compare with captured parent"));
        assert!(current.contains(&diff_url(
            fixture.older_revision,
            fixture.current_revision,
            fixture.wiki_id
        )));
        assert!(current.contains(&article_url(&fixture.title, fixture.wiki_id)));

        let (_, _, older) = response_text(
            router(&fixture.root),
            &format!(
                "/revision/{}?wiki={}",
                fixture.older_revision, fixture.wiki_id
            ),
        )
        .await;
        assert!(older.contains("Newer captured revision"));
    }

    #[tokio::test]
    async fn provenance_language_matches_the_verified_read_boundary() {
        let fixture = fixture();
        let title = utf8_percent_encode(fixture.title.as_str(), NON_ALPHANUMERIC);
        let (_, _, article) = response_text(
            router(&fixture.root),
            &format!("/wiki/{title}?wiki={}", fixture.wiki_id),
        )
        .await;
        assert!(article.contains("bytes were checked against"));
        assert!(article.contains("does not verify that the content is true"));
        assert!(article.contains("Licensing and attribution requirements depend on"));
        assert!(!article.contains("verified as true"));

        let (_, _, about) =
            response_text(router(&fixture.root), "/about/source-and-integrity").await;
        assert!(about.contains("not by itself a full-library or manifest-chain verification"));
        assert!(about.contains("privacy, copyright, or safety obligations"));
    }

    #[tokio::test]
    async fn reader_shell_is_responsive_accessible_and_uses_only_bundled_styles() {
        let fixture = fixture();
        let (_, _, home) = response_text(router(&fixture.root), "/").await;
        assert!(home.contains("class=\"skip-link\" href=\"#content\""));
        assert!(home.contains("<main id=\"content\">"));
        assert!(home.contains("aria-label=\"Library\""));

        let (_, _, css) = response_text(router(&fixture.root), "/assets/reader.css").await;
        assert!(css.contains("@media (max-width: 44rem)"));
        assert!(css.contains("@media (prefers-color-scheme: dark)"));
        assert!(css.contains(".footnote-definition"));
        assert!(!css.contains("url("));
    }

    #[test]
    fn reference_rewrite_handles_nested_markdown_links_and_malformed_input() {
        let markdown = "Text [ref: See [guide](https://example.test/a).]\n";
        let rewritten = reader_markdown(markdown);
        assert!(rewritten.contains("Text [^reader-reference-1]"));
        assert!(rewritten.contains("## References"));
        assert!(rewritten.contains("[^reader-reference-1]: See [guide](https://example.test/a)."));
        assert_eq!(
            reader_markdown("Text [ref: unfinished"),
            "Text [ref: unfinished"
        );
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
            let (status, headers, bytes) = response_bytes(router(&fixture.root), &uri).await;
            assert!(
                status.is_success() || status == StatusCode::NOT_FOUND,
                "crawl failed for {uri}: {status}"
            );
            if uri.starts_with("/media/") {
                assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "image/png");
                assert_eq!(bytes, VALID_PNG);
                continue;
            }
            let body = String::from_utf8(bytes).expect("HTML or CSS is UTF-8");
            for resource in attribute_values(&body, "src")
                .into_iter()
                .chain(stylesheet_links(&body))
            {
                assert!(
                    resource.starts_with('/') || resource.starts_with("data:"),
                    "outbound resource URL in {uri}: {resource}"
                );
                if resource.starts_with('/') {
                    pending.push(resource);
                }
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
        assert!(visited.iter().any(|path| path.starts_with("/media/")));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ephemeral_reader_is_ready_and_shuts_down_gracefully() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpStream;
        use std::time::Duration;

        let fixture = fixture();
        let reader = start_loopback(&fixture.root).await.expect("start reader");
        let address = reader.address();
        assert!(address.ip().is_loopback());
        assert_ne!(address.port(), 0);
        assert_eq!(reader.local_url(), format!("http://{address}/"));

        let response = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
                .expect("reader accepts connections once returned");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .expect("HTTP request");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("HTTP response");
            response
        })
        .await
        .expect("request task");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Your offline encyclopedia"));

        reader.shutdown().await.expect("graceful shutdown");
        assert!(
            TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err(),
            "listener remained reachable after shutdown completed"
        );
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
