mod doctor;

use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str;

use serde_json::json;
use wikisync_content::{
    ContentDiff, DiffLine, DiffMode, DiffTag, diff as content_diff, to_markdown, to_plain_text,
};
use wikisync_core::{CollectionId, PageTitle, RevisionId, WikiId};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_search::{
    MAX_SEARCH_RESULTS, SearchError, SearchIndex, SearchQuery, SqliteSearchIndex,
};
use wikisync_store::{
    Library, StoreError, StoredPage, StoredRevision, SyncCheckpoint, SyncRunState, SyncRunStatus,
};
use wikisync_sync::{CategoryPreviewLimits, preview_category_selection};
use wikisyncd::{ApplicationHandler, Mutation, OperationControl, RequestHandler, WriterAccess};

const USAGE: &str = "WikiSyncer offline reader

Usage:
  wikisync category-preview --api-endpoint <url> [--depth <edges>] [--json] <Category:title>
  wikisync --library <path> search [--wiki <id>] [--limit <count>] [--json] <query>
  wikisync --library <path> show [--wiki <id>] [--revision <id>] [--json] [--source] <title>
  wikisync --library <path> history [--wiki <id>] [--json] <title>
  wikisync --library <path> diff [--wiki <id>] [--reading] [--json] <from-revision> <to-revision>
  wikisync --library <path> sync [--collection <id>]
  wikisync --library <path> verify [--full]
  wikisync --library <path> compact
  wikisync --library <path> status [--json]
  wikisync --library <path> doctor [--json] [--bundle <new-file>]
  wikisync --library <path> serve [--port <port>]
  wikisync --help
  wikisync --version

The WIKISYNC_LIBRARY environment variable may replace --library.

category-preview is network-only and does not change collection membership. It selects
only main-namespace pages, traverses namespace-14 subcategories, and defaults to bounds
of 16 levels, 1,000 categories, 10,000 pages, and 20,000 API responses.";

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wikisync: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(arguments: impl IntoIterator<Item = impl Into<std::ffi::OsString>>) -> Result<(), CliError> {
    match parse(arguments)? {
        Action::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Action::Version => {
            println!("wikisync {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::CategoryPreview {
            api_endpoint,
            category,
            depth,
            json,
        } => category_preview(&api_endpoint, &category, depth, json),
        Action::Command { library, command } => {
            if !library.join("library.sqlite3").is_file() {
                return Err(CliError::message(format!(
                    "{} is not an initialized WikiSyncer library",
                    library.display()
                )));
            }
            let library_root = library;
            if let Command::Doctor { json, bundle } = &command {
                return doctor::run(&library_root, *json, bundle.as_deref()).map_err(Into::into);
            }
            match command {
                Command::Sync { collection_id } => {
                    let mutation = collection_id
                        .map_or(Mutation::SyncAll, |id| Mutation::SyncCollection(id.get()));
                    return mutate_library(&library_root, mutation);
                }
                Command::Verify { full } => {
                    return mutate_library(&library_root, Mutation::Verify { full });
                }
                Command::Compact => return mutate_library(&library_root, Mutation::Compact),
                _ => {}
            }
            let library = Library::open(&library_root)?;
            match command {
                Command::Search {
                    query,
                    wiki_id,
                    limit,
                    json,
                } => search(&library, &query, wiki_id, limit, json),
                Command::Show {
                    title,
                    wiki_id,
                    revision_id,
                    json,
                    source,
                } => show(&library, &title, wiki_id, revision_id, json, source),
                Command::History {
                    title,
                    wiki_id,
                    json,
                } => history(&library, &title, wiki_id, json),
                Command::Diff {
                    from,
                    to,
                    wiki_id,
                    reading,
                    json,
                } => revision_diff(&library, from, to, wiki_id, reading, json),
                Command::Status { json } => status(&library, json),
                Command::Serve { port } => {
                    drop(library);
                    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
                    println!("WikiSyncer reader available at http://{address}/");
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(wikisync_web::serve(library_root, address))?;
                    Ok(())
                }
                Command::Sync { .. } | Command::Verify { .. } | Command::Compact => {
                    unreachable!("mutating commands returned before opening a reader")
                }
                Command::Doctor { .. } => {
                    unreachable!("doctor returned before opening the normal reader")
                }
            }
        }
    }
}

fn mutate_library(library_root: &std::path::Path, mutation: Mutation) -> Result<(), CliError> {
    let outcome = match WriterAccess::discover(library_root)? {
        WriterAccess::Daemon(client) => client.forward_mutation(mutation)?,
        WriterAccess::Direct(_lease) => {
            let mut handler = ApplicationHandler::new(library_root)?;
            handler.mutate(mutation, OperationControl::running())?
        }
    };
    println!("{}", outcome.result);
    if !outcome.payload.is_empty() {
        let detail = str::from_utf8(&outcome.payload)
            .map_err(|_| CliError::data("daemon mutation receipt is not valid UTF-8"))?;
        println!("{detail}");
    }
    Ok(())
}

fn category_preview(
    api_endpoint: &str,
    category: &PageTitle,
    depth: u16,
    json_output: bool,
) -> Result<(), CliError> {
    let config = ClientConfig::new(
        api_endpoint,
        format!(
            "WikiSyncer/{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        ),
    )?;
    let client = MediaWikiClient::new(config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let limits = CategoryPreviewLimits::default();
    let preview = runtime.block_on(preview_category_selection(&client, category, depth, limits))?;

    if json_output {
        write_json(&json!({
            "root": preview.root.as_str(),
            "recursion_depth": preview.recursion_depth,
            "page_count": preview.pages.len(),
            "category_count": preview.categories.len(),
            "batches": preview.batches,
            "limits": {
                "max_recursion_depth": limits.max_recursion_depth,
                "max_categories": limits.max_categories,
                "max_pages": limits.max_pages,
                "max_batches": limits.max_batches,
            },
            "categories": preview.categories.iter().map(|category| json!({
                "title": category.title.as_str(),
                "depth": category.depth,
            })).collect::<Vec<_>>(),
            "pages": preview.pages.iter().map(|page| json!({
                "page_id": page.page_id.get(),
                "namespace": page.namespace,
                "title": page.title.as_str(),
                "category_depth": page.category_depth,
            })).collect::<Vec<_>>(),
        }))?;
    } else {
        println!(
            "Category preview: {} (depth {}, {} pages, {} categories, {} API responses)",
            preview.root,
            preview.recursion_depth,
            preview.pages.len(),
            preview.categories.len(),
            preview.batches,
        );
        println!(
            "Bounds: depth {}, categories {}, pages {}, API responses {}",
            limits.max_recursion_depth, limits.max_categories, limits.max_pages, limits.max_batches,
        );
        for page in preview.pages {
            println!("{}\t{}", page.page_id, page.title);
        }
    }
    Ok(())
}

fn status(library: &Library, json_output: bool) -> Result<(), CliError> {
    let checkpoints = library.sync_checkpoints()?;
    let runs = library.sync_run_statuses(20)?;
    let state = library_sync_state(&runs);
    if json_output {
        write_json(&json!({
            "state": state,
            "checkpoints": checkpoints.iter().map(checkpoint_json).collect::<Vec<_>>(),
            "runs": runs.iter().map(sync_run_json).collect::<Vec<_>>(),
        }))?;
    } else {
        println!("WikiSyncer status: {state}");
        if checkpoints.is_empty() {
            println!("No source checkpoints recorded.");
        } else {
            for checkpoint in checkpoints {
                let scope = checkpoint.collection_id.map_or_else(
                    || "all collections".to_owned(),
                    |id| format!("collection {id}"),
                );
                println!(
                    "Wiki {} ({scope}): committed through {}, next window starts {} ({}s overlap)",
                    checkpoint.wiki_id,
                    checkpoint.committed_through,
                    checkpoint.next_window_start(),
                    checkpoint.overlap_seconds,
                );
            }
        }
        for run in runs {
            println!(
                "Run {}: {} {} — {} queued, {} running, {} succeeded, {} failed",
                run.run_id,
                run.kind.as_str(),
                run.state.as_str(),
                run.queued_jobs,
                run.running_jobs,
                run.succeeded_jobs,
                run.failed_jobs,
            );
            if let Some(error) = run.latest_error {
                println!("  Last error [{}]: {}", error.code, error.message);
            }
        }
    }
    Ok(())
}

fn library_sync_state(runs: &[SyncRunStatus]) -> &'static str {
    if runs
        .iter()
        .any(|run| run.state == SyncRunState::Running && run.failed_jobs > 0)
    {
        "attention"
    } else if runs.iter().any(|run| run.state == SyncRunState::Running) {
        "running"
    } else {
        "idle"
    }
}

fn checkpoint_json(checkpoint: &SyncCheckpoint) -> serde_json::Value {
    json!({
        "wiki_id": checkpoint.wiki_id.get(),
        "collection_id": checkpoint.collection_id.map(|id| id.get()),
        "committed_through": checkpoint.committed_through,
        "overlap_seconds": checkpoint.overlap_seconds,
        "next_window_start": checkpoint.next_window_start(),
        "recent_changes_cursor": checkpoint.recent_changes_cursor,
        "reconciled_at": checkpoint.reconciled_at,
        "last_run_id": checkpoint.last_run_id,
        "updated_at": checkpoint.updated_at,
    })
}

fn sync_run_json(run: &SyncRunStatus) -> serde_json::Value {
    json!({
        "run_id": run.run_id,
        "wiki_id": run.wiki_id.get(),
        "collection_id": run.collection_id.map(|id| id.get()),
        "kind": run.kind.as_str(),
        "state": run.state.as_str(),
        "window_start": run.window_start,
        "checkpoint_candidate": run.checkpoint_candidate,
        "jobs": {
            "queued": run.queued_jobs,
            "running": run.running_jobs,
            "succeeded": run.succeeded_jobs,
            "failed": run.failed_jobs,
        },
        "created_at": run.created_at,
        "finished_at": run.finished_at,
        "latest_error": run.latest_error.as_ref().map(|error| json!({
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "occurred_at": error.occurred_at,
        })),
    })
}

fn search(
    library: &Library,
    query_text: &str,
    wiki_id: Option<WikiId>,
    limit: u32,
    json_output: bool,
) -> Result<(), CliError> {
    let index = SqliteSearchIndex::open(library)?;
    let mut query = SearchQuery::new(query_text).with_limit(limit);
    if let Some(wiki_id) = wiki_id {
        query = query.for_wiki(wiki_id);
    }
    let hits = index.search(query)?;
    let mut results = Vec::with_capacity(hits.len());
    for hit in hits {
        let revision = library
            .revision(hit.wiki_id, hit.revision_id)?
            .ok_or_else(|| CliError::data("search index points to a missing revision"))?;
        let source = canonical_source(library, &revision)?;
        let body = to_plain_text(&source);
        results.push((hit, excerpt(&body, query_text, 200)));
    }

    if json_output {
        let values = results
            .into_iter()
            .map(|(hit, excerpt)| {
                json!({
                    "wiki_id": hit.wiki_id.get(),
                    "page_id": hit.page_id.get(),
                    "revision_id": hit.revision_id.get(),
                    "title": hit.title.as_str(),
                    "rank": hit.rank,
                    "excerpt": excerpt,
                })
            })
            .collect::<Vec<_>>();
        write_json(&serde_json::Value::Array(values))?;
    } else {
        for (position, (hit, excerpt)) in results.into_iter().enumerate() {
            if position > 0 {
                println!();
            }
            println!(
                "{}  [wiki {}, page {}, revision {}]",
                hit.title, hit.wiki_id, hit.page_id, hit.revision_id
            );
            if !excerpt.is_empty() {
                println!("  {excerpt}");
            }
        }
    }
    Ok(())
}

fn show(
    library: &Library,
    title: &PageTitle,
    wiki_id: Option<WikiId>,
    selected_revision: Option<RevisionId>,
    json_output: bool,
    exact_source: bool,
) -> Result<(), CliError> {
    let matches = library.pages_by_title(title, wiki_id)?;
    let page = unique_page(matches, title, wiki_id)?;
    let revision_id = selected_revision
        .or(page.current_revision_id)
        .ok_or_else(|| CliError::data("captured page has no current revision"))?;
    let revision = library
        .revision(page.wiki_id, revision_id)?
        .ok_or_else(|| CliError::message(format!("revision {revision_id} was not found")))?;
    if revision.page_id != page.page_id {
        return Err(CliError::message(format!(
            "revision {revision_id} does not belong to page {}",
            page.page_id
        )));
    }
    let source = canonical_source(library, &revision)?;
    let (format, body) = if exact_source {
        ("wikitext", source)
    } else {
        ("markdown", to_markdown(&source))
    };

    if json_output {
        write_json(&json!({
            "wiki_id": page.wiki_id.get(),
            "page_id": page.page_id.get(),
            "namespace": page.namespace,
            "title": page.title.as_str(),
            "revision_id": revision.revision_id.get(),
            "parent_revision_id": revision.parent_id.map(|id| id.get()),
            "revision_time": revision.timestamp,
            "content_object_id": revision.content_object_id.to_string(),
            "format": format,
            "content": body,
        }))?;
    } else {
        println!("# {}", page.title);
        println!();
        println!("Wiki: {}", page.wiki_id);
        println!("Page: {}", page.page_id);
        println!("Revision: {}", revision.revision_id);
        println!("Revision time: {}", revision.timestamp);
        println!("Content object: {}", revision.content_object_id);
        if !body.is_empty() {
            println!();
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

fn history(
    library: &Library,
    title: &PageTitle,
    wiki_id: Option<WikiId>,
    json_output: bool,
) -> Result<(), CliError> {
    let page = unique_page(library.pages_by_title(title, wiki_id)?, title, wiki_id)?;
    let revisions = library.revisions_for_page(page.wiki_id, page.page_id)?;
    if json_output {
        write_json(&json!({
            "wiki_id": page.wiki_id.get(),
            "page_id": page.page_id.get(),
            "title": page.title.as_str(),
            "current_revision_id": page.current_revision_id.map(|id| id.get()),
            "revisions": revisions.iter().map(revision_json).collect::<Vec<_>>(),
        }))?;
    } else {
        println!(
            "History: {}  [wiki {}, page {}]",
            page.title, page.wiki_id, page.page_id
        );
        for revision in revisions {
            let current = if page.current_revision_id == Some(revision.revision_id) {
                " *"
            } else {
                ""
            };
            let author = revision.author.as_deref().unwrap_or("hidden");
            println!(
                "{}  {}  {}{}",
                revision.revision_id, revision.timestamp, author, current
            );
            if let Some(comment) = revision.comment.as_deref()
                && !comment.is_empty()
            {
                println!("  {comment}");
            }
        }
    }
    Ok(())
}

fn revision_diff(
    library: &Library,
    from_id: RevisionId,
    to_id: RevisionId,
    wiki_id: Option<WikiId>,
    reading: bool,
    json_output: bool,
) -> Result<(), CliError> {
    let (from_wiki, from) = unique_revision(library, from_id, wiki_id)?;
    let (to_wiki, to) = unique_revision(library, to_id, wiki_id)?;
    if from_wiki != to_wiki {
        return Err(CliError::message(
            "diff revisions must belong to the same wiki",
        ));
    }
    if from.page_id != to.page_id {
        return Err(CliError::message(
            "diff revisions must belong to the same page",
        ));
    }
    let older = canonical_source(library, &from)?;
    let newer = canonical_source(library, &to)?;
    let mode = if reading {
        DiffMode::Reading
    } else {
        DiffMode::ExactSource
    };
    let comparison = content_diff(&older, &newer, mode);

    if json_output {
        write_json(&json!({
            "wiki_id": from_wiki.get(),
            "page_id": from.page_id.get(),
            "from_revision_id": from.revision_id.get(),
            "to_revision_id": to.revision_id.get(),
            "mode": comparison.mode.as_str(),
            "has_changes": comparison.has_changes(),
            "lines": comparison.lines.iter().map(diff_line_json).collect::<Vec<_>>(),
        }))?;
    } else {
        print_human_diff(&from, &to, &comparison)?;
    }
    Ok(())
}

fn revision_json(revision: &StoredRevision) -> serde_json::Value {
    json!({
        "revision_id": revision.revision_id.get(),
        "parent_revision_id": revision.parent_id.map(|id| id.get()),
        "revision_time": revision.timestamp,
        "author": revision.author,
        "author_id": revision.author_id,
        "comment": revision.comment,
        "minor": revision.minor,
        "source_size": revision.source_size,
        "upstream_sha1": revision.upstream_sha1,
        "content_model": revision.content_model,
        "content_object_id": revision.content_object_id.to_string(),
        "captured_at": revision.captured_at,
    })
}

fn diff_line_json(line: &DiffLine) -> serde_json::Value {
    json!({
        "tag": line.tag.as_str(),
        "old_line": line.old_line,
        "new_line": line.new_line,
        "spans": line.spans.iter().map(|span| json!({
            "tag": span.tag.as_str(),
            "text": span.text,
        })).collect::<Vec<_>>(),
    })
}

fn print_human_diff(
    from: &StoredRevision,
    to: &StoredRevision,
    comparison: &ContentDiff,
) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "--- revision {} ({})",
        from.revision_id,
        comparison.mode.as_str()
    )?;
    writeln!(output, "+++ revision {}", to.revision_id)?;
    for line in &comparison.lines {
        let prefix = match line.tag {
            DiffTag::Equal => ' ',
            DiffTag::Delete => '-',
            DiffTag::Insert => '+',
        };
        write!(output, "{prefix}")?;
        let ends_in_newline = line
            .spans
            .last()
            .is_some_and(|span| span.text.ends_with('\n'));
        for (index, span) in line.spans.iter().enumerate() {
            let mut text = span.text.as_str();
            if index + 1 == line.spans.len() && ends_in_newline {
                text = text.strip_suffix('\n').expect("checked line ending");
                text = text.strip_suffix('\r').unwrap_or(text);
            }
            match span.tag {
                DiffTag::Delete if line.tag == DiffTag::Delete => {
                    write!(output, "[-{text}-]")?;
                }
                DiffTag::Insert if line.tag == DiffTag::Insert => {
                    write!(output, "{{+{text}+}}")?;
                }
                _ => write!(output, "{text}")?,
            }
        }
        writeln!(output)?;
        if !ends_in_newline {
            writeln!(output, "\\ No newline at end of revision")?;
        }
    }
    Ok(())
}

fn canonical_source(library: &Library, revision: &StoredRevision) -> Result<String, CliError> {
    let bytes = library.read_object(revision.content_object_id)?;
    String::from_utf8(bytes)
        .map_err(|_| CliError::data("canonical wikitext object is not valid UTF-8"))
}

fn unique_revision(
    library: &Library,
    revision_id: RevisionId,
    wiki_id: Option<WikiId>,
) -> Result<(WikiId, StoredRevision), CliError> {
    if let Some(wiki_id) = wiki_id {
        return library
            .revision(wiki_id, revision_id)?
            .map(|revision| (wiki_id, revision))
            .ok_or_else(|| {
                CliError::message(format!(
                    "revision {revision_id} was not found in wiki {wiki_id}"
                ))
            });
    }
    let mut matches = library.revisions_by_id(revision_id)?;
    match matches.len() {
        0 => Err(CliError::message(format!(
            "revision {revision_id} was not found"
        ))),
        1 => Ok(matches.remove(0)),
        count => Err(CliError::message(format!(
            "revision {revision_id} matched {count} wikis; use --wiki <id> to select one source"
        ))),
    }
}

fn unique_page(
    mut matches: Vec<StoredPage>,
    title: &PageTitle,
    wiki_id: Option<WikiId>,
) -> Result<StoredPage, CliError> {
    match matches.len() {
        0 => Err(CliError::message(format!("page {title} was not found"))),
        1 => Ok(matches.remove(0)),
        count => {
            let wiki_hint = wiki_id.map_or_else(
                || "use --wiki <id> to select one source".to_owned(),
                |wiki_id| format!("wiki {wiki_id} contains conflicting page matches"),
            );
            Err(CliError::message(format!(
                "page {title} matched {count} pages; {wiki_hint}"
            )))
        }
    }
}

fn excerpt(body: &str, query: &str, maximum_chars: usize) -> String {
    let terms = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| term.len() > 1)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let line = body
        .lines()
        .find(|line| {
            let lowercase = line.to_lowercase();
            terms.iter().any(|term| lowercase.contains(term))
        })
        .or_else(|| body.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() <= maximum_chars {
        line
    } else {
        let mut truncated = line.chars().take(maximum_chars - 1).collect::<String>();
        truncated.push('…');
        truncated
    }
}

fn write_json(value: &serde_json::Value) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Help,
    Version,
    CategoryPreview {
        api_endpoint: String,
        category: PageTitle,
        depth: u16,
        json: bool,
    },
    Command {
        library: PathBuf,
        command: Command,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Command {
    Search {
        query: String,
        wiki_id: Option<WikiId>,
        limit: u32,
        json: bool,
    },
    Show {
        title: PageTitle,
        wiki_id: Option<WikiId>,
        revision_id: Option<RevisionId>,
        json: bool,
        source: bool,
    },
    History {
        title: PageTitle,
        wiki_id: Option<WikiId>,
        json: bool,
    },
    Diff {
        from: RevisionId,
        to: RevisionId,
        wiki_id: Option<WikiId>,
        reading: bool,
        json: bool,
    },
    Sync {
        collection_id: Option<CollectionId>,
    },
    Verify {
        full: bool,
    },
    Compact,
    Status {
        json: bool,
    },
    Doctor {
        json: bool,
        bundle: Option<PathBuf>,
    },
    Serve {
        port: u16,
    },
}

fn parse(
    arguments: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> Result<Action, CliError> {
    let mut arguments = arguments
        .into_iter()
        .map(Into::into)
        .collect::<Vec<_>>()
        .into_iter();
    let mut library = env::var_os("WIKISYNC_LIBRARY").map(PathBuf::from);
    let command = loop {
        let Some(argument) = arguments.next() else {
            return Err(CliError::usage("a command is required"));
        };
        match argument.to_str() {
            Some("--library") => {
                library =
                    Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        CliError::usage("--library requires a path")
                    })?));
            }
            Some("--help" | "-h") => return Ok(Action::Help),
            Some("--version" | "-V") => return Ok(Action::Version),
            Some(
                "category-preview" | "search" | "show" | "history" | "diff" | "sync" | "verify"
                | "compact" | "status" | "doctor" | "serve",
            ) => break argument,
            Some(value) => return Err(CliError::usage(format!("unknown command {value:?}"))),
            None => return Err(CliError::usage("arguments must be valid UTF-8")),
        }
    };
    let values = arguments
        .map(|value| {
            value
                .into_string()
                .map_err(|_| CliError::usage("arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if command.to_str() == Some("category-preview") {
        return parse_category_preview(values);
    }
    let library = library.ok_or_else(|| {
        CliError::usage("--library <path> or WIKISYNC_LIBRARY is required for offline commands")
    })?;
    let command = match command.to_str() {
        Some("search") => parse_search(values)?,
        Some("show") => parse_show(values)?,
        Some("history") => parse_history(values)?,
        Some("diff") => parse_diff(values)?,
        Some("sync") => parse_sync(values)?,
        Some("verify") => parse_verify(values)?,
        Some("compact") => parse_compact(values)?,
        Some("status") => parse_status(values)?,
        Some("doctor") => parse_doctor(values)?,
        Some("serve") => parse_serve(values)?,
        _ => unreachable!("validated command"),
    };
    Ok(Action::Command { library, command })
}

fn parse_category_preview(values: Vec<String>) -> Result<Action, CliError> {
    let mut api_endpoint = None;
    let mut depth = 0_u16;
    let mut json = false;
    let mut category = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--api-endpoint" => api_endpoint = Some(required_value(&mut values, "--api-endpoint")?),
            "--depth" => {
                depth = required_value(&mut values, "--depth")?
                    .parse::<u16>()
                    .map_err(|_| CliError::usage("--depth requires an integer from 0 to 65535"))?;
            }
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!(
                    "unknown category-preview option {value:?}"
                )));
            }
            _ => category.push(value),
        }
    }
    let api_endpoint = api_endpoint
        .ok_or_else(|| CliError::usage("category-preview requires --api-endpoint <url>"))?;
    let category = PageTitle::new(category.join(" ")).map_err(|error| {
        CliError::usage(format!(
            "category-preview requires a fully qualified category title: {error}"
        ))
    })?;
    Ok(Action::CategoryPreview {
        api_endpoint,
        category,
        depth,
        json,
    })
}

fn parse_serve(values: Vec<String>) -> Result<Command, CliError> {
    let mut port = 8_080;
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--port" => {
                port = required_value(&mut values, "--port")?
                    .parse::<u16>()
                    .map_err(|_| CliError::usage("--port requires an integer from 1 to 65535"))?;
                if port == 0 {
                    return Err(CliError::usage(
                        "--port requires an integer from 1 to 65535",
                    ));
                }
            }
            _ => return Err(CliError::usage(format!("unknown serve option {value:?}"))),
        }
    }
    Ok(Command::Serve { port })
}

fn parse_status(values: Vec<String>) -> Result<Command, CliError> {
    let mut json = false;
    for value in values {
        match value.as_str() {
            "--json" => json = true,
            _ => return Err(CliError::usage(format!("unknown status option {value:?}"))),
        }
    }
    Ok(Command::Status { json })
}

fn parse_doctor(values: Vec<String>) -> Result<Command, CliError> {
    let mut json = false;
    let mut bundle = None;
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--json" => json = true,
            "--bundle" => {
                let path = PathBuf::from(required_value(&mut values, "--bundle")?);
                if bundle.replace(path).is_some() {
                    return Err(CliError::usage("--bundle may only be supplied once"));
                }
            }
            _ => return Err(CliError::usage(format!("unknown doctor option {value:?}"))),
        }
    }
    Ok(Command::Doctor { json, bundle })
}

fn parse_sync(values: Vec<String>) -> Result<Command, CliError> {
    let mut collection_id = None;
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--collection" => {
                let raw = required_value(&mut values, "--collection")?
                    .parse::<u64>()
                    .map_err(|_| CliError::usage("--collection requires a positive integer"))?;
                let parsed =
                    CollectionId::new(raw).map_err(|error| CliError::usage(error.to_string()))?;
                if collection_id.replace(parsed).is_some() {
                    return Err(CliError::usage("--collection may only be supplied once"));
                }
            }
            _ => return Err(CliError::usage(format!("unknown sync option {value:?}"))),
        }
    }
    Ok(Command::Sync { collection_id })
}

fn parse_verify(values: Vec<String>) -> Result<Command, CliError> {
    let mut full = false;
    for value in values {
        match value.as_str() {
            "--full" => full = true,
            _ => return Err(CliError::usage(format!("unknown verify option {value:?}"))),
        }
    }
    Ok(Command::Verify { full })
}

fn parse_compact(values: Vec<String>) -> Result<Command, CliError> {
    if let Some(value) = values.first() {
        return Err(CliError::usage(format!("unknown compact option {value:?}")));
    }
    Ok(Command::Compact)
}

fn parse_search(values: Vec<String>) -> Result<Command, CliError> {
    let mut wiki_id = None;
    let mut limit = 20;
    let mut json = false;
    let mut query = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--wiki" => wiki_id = Some(parse_wiki(required_value(&mut values, "--wiki")?)?),
            "--limit" => {
                let value = required_value(&mut values, "--limit")?;
                limit = value
                    .parse::<u32>()
                    .map_err(|_| CliError::usage("--limit requires a positive integer"))?;
                if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
                    return Err(CliError::usage(format!(
                        "--limit must be between 1 and {MAX_SEARCH_RESULTS}"
                    )));
                }
            }
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!("unknown search option {value:?}")));
            }
            _ => query.push(value),
        }
    }
    if query.is_empty() {
        return Err(CliError::usage("search requires a query"));
    }
    Ok(Command::Search {
        query: query.join(" "),
        wiki_id,
        limit,
        json,
    })
}

fn parse_show(values: Vec<String>) -> Result<Command, CliError> {
    let mut wiki_id = None;
    let mut revision_id = None;
    let mut json = false;
    let mut source = false;
    let mut title = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--wiki" => wiki_id = Some(parse_wiki(required_value(&mut values, "--wiki")?)?),
            "--revision" => {
                revision_id = Some(parse_revision(required_value(&mut values, "--revision")?)?)
            }
            "--json" => json = true,
            "--source" => source = true,
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!("unknown show option {value:?}")));
            }
            _ => title.push(value),
        }
    }
    let title = PageTitle::new(title.join(" "))
        .map_err(|error| CliError::usage(format!("show requires a valid title: {error}")))?;
    Ok(Command::Show {
        title,
        wiki_id,
        revision_id,
        json,
        source,
    })
}

fn parse_history(values: Vec<String>) -> Result<Command, CliError> {
    let mut wiki_id = None;
    let mut json = false;
    let mut title = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--wiki" => wiki_id = Some(parse_wiki(required_value(&mut values, "--wiki")?)?),
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!("unknown history option {value:?}")));
            }
            _ => title.push(value),
        }
    }
    let title = PageTitle::new(title.join(" "))
        .map_err(|error| CliError::usage(format!("history requires a valid title: {error}")))?;
    Ok(Command::History {
        title,
        wiki_id,
        json,
    })
}

fn parse_diff(values: Vec<String>) -> Result<Command, CliError> {
    let mut wiki_id = None;
    let mut reading = false;
    let mut json = false;
    let mut revisions = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--wiki" => wiki_id = Some(parse_wiki(required_value(&mut values, "--wiki")?)?),
            "--reading" => reading = true,
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(CliError::usage(format!("unknown diff option {value:?}")));
            }
            _ => revisions.push(parse_revision(value)?),
        }
    }
    if revisions.len() != 2 {
        return Err(CliError::usage("diff requires exactly two revision IDs"));
    }
    Ok(Command::Diff {
        from: revisions[0],
        to: revisions[1],
        wiki_id,
        reading,
        json,
    })
}

fn required_value(
    values: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, CliError> {
    values
        .next()
        .ok_or_else(|| CliError::usage(format!("{option} requires a value")))
}

fn parse_wiki(value: String) -> Result<WikiId, CliError> {
    let value = value
        .parse::<u64>()
        .map_err(|_| CliError::usage("--wiki requires a positive integer"))?;
    WikiId::new(value).map_err(|error| CliError::usage(error.to_string()))
}

fn parse_revision(value: String) -> Result<RevisionId, CliError> {
    let value = value
        .parse::<u64>()
        .map_err(|_| CliError::usage("revision ID must be a positive integer"))?;
    RevisionId::new(value).map_err(|error| CliError::usage(error.to_string()))
}

#[derive(Debug)]
struct CliError {
    message: String,
    show_usage: bool,
}

impl CliError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            show_usage: false,
        }
    }

    fn data(message: &'static str) -> Self {
        Self::message(format!("corrupt library: {message}"))
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            show_usage: true,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)?;
        if self.show_usage {
            write!(formatter, "\n\n{USAGE}")?;
        }
        Ok(())
    }
}

impl Error for CliError {}

impl From<StoreError> for CliError {
    fn from(error: StoreError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<SearchError> for CliError {
    fn from(error: SearchError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::message(error.to_string())
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisync_web::ServeError> for CliError {
    fn from(error: wikisync_web::ServeError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisyncd::DaemonError> for CliError {
    fn from(error: wikisyncd::DaemonError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisyncd::OperationError> for CliError {
    fn from(error: wikisyncd::OperationError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<doctor::DoctorError> for CliError {
    fn from(error: doctor::DoctorError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisync_mediawiki::ConfigError> for CliError {
    fn from(error: wikisync_mediawiki::ConfigError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisync_mediawiki::ClientError> for CliError {
    fn from(error: wikisync_mediawiki::ClientError) -> Self {
        Self::message(error.to_string())
    }
}

impl From<wikisync_sync::CategoryPreviewError> for CliError {
    fn from(error: wikisync_sync::CategoryPreviewError) -> Self {
        Self::message(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_options_and_joined_query() {
        let action = parse([
            "--library",
            "/tmp/wiki",
            "search",
            "--wiki",
            "2",
            "--limit",
            "7",
            "memory",
            "safety",
            "--json",
        ])
        .expect("parse");
        assert_eq!(
            action,
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Search {
                    query: "memory safety".to_owned(),
                    wiki_id: Some(WikiId::new(2).expect("wiki")),
                    limit: 7,
                    json: true,
                }
            }
        );
    }

    #[test]
    fn parses_standalone_category_preview_without_a_library() {
        let action = parse([
            "category-preview",
            "--api-endpoint",
            "https://en.wikipedia.org/w/api.php",
            "--depth",
            "2",
            "--json",
            "Category:Rust",
        ])
        .expect("category preview");
        assert_eq!(
            action,
            Action::CategoryPreview {
                api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
                category: PageTitle::new("Category:Rust").expect("category"),
                depth: 2,
                json: true,
            }
        );
    }

    #[test]
    fn parses_history_show_revision_and_reading_diff() {
        let history = parse([
            "--library",
            "/tmp/wiki",
            "history",
            "--wiki",
            "2",
            "Rust",
            "language",
            "--json",
        ])
        .expect("history parse");
        assert_eq!(
            history,
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::History {
                    title: PageTitle::new("Rust language").expect("title"),
                    wiki_id: Some(WikiId::new(2).expect("wiki")),
                    json: true,
                },
            }
        );

        let show = parse(["--library", "/tmp/wiki", "show", "--revision", "41", "Rust"])
            .expect("show parse");
        assert!(matches!(
            show,
            Action::Command {
                command: Command::Show {
                    revision_id: Some(id),
                    ..
                },
                ..
            } if id == RevisionId::new(41).expect("revision")
        ));

        let comparison = parse([
            "--library",
            "/tmp/wiki",
            "diff",
            "--reading",
            "--json",
            "40",
            "41",
        ])
        .expect("diff parse");
        assert_eq!(
            comparison,
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Diff {
                    from: RevisionId::new(40).expect("from"),
                    to: RevisionId::new(41).expect("to"),
                    wiki_id: None,
                    reading: true,
                    json: true,
                },
            }
        );

        assert_eq!(
            parse(["--library", "/tmp/wiki", "status", "--json"]).expect("status parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Status { json: true },
            }
        );

        assert_eq!(
            parse(["--library", "/tmp/wiki", "serve", "--port", "8765"]).expect("serve parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Serve { port: 8_765 },
            }
        );
    }

    #[test]
    fn parses_daemon_aware_writer_commands() {
        assert_eq!(
            parse(["--library", "/tmp/wiki", "sync", "--collection", "7"]).expect("sync parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Sync {
                    collection_id: Some(CollectionId::new(7).expect("collection")),
                },
            }
        );
        assert_eq!(
            parse(["--library", "/tmp/wiki", "verify", "--full"]).expect("verify parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Verify { full: true },
            }
        );
        assert_eq!(
            parse(["--library", "/tmp/wiki", "compact"]).expect("compact parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Compact,
            }
        );
    }

    #[test]
    fn parses_offline_doctor_outputs() {
        assert_eq!(
            parse([
                "--library",
                "/tmp/wiki",
                "doctor",
                "--json",
                "--bundle",
                "/tmp/doctor.json",
            ])
            .expect("doctor parse"),
            Action::Command {
                library: PathBuf::from("/tmp/wiki"),
                command: Command::Doctor {
                    json: true,
                    bundle: Some(PathBuf::from("/tmp/doctor.json")),
                },
            }
        );
        assert!(
            parse([
                "--library",
                "/tmp/wiki",
                "doctor",
                "--bundle",
                "one",
                "--bundle",
                "two",
            ])
            .is_err()
        );
    }

    #[test]
    fn excerpt_prefers_a_matching_line_and_bounds_characters() {
        assert_eq!(
            excerpt("Heading\nA memory-safe systems language.\n", "memory", 80),
            "A memory-safe systems language."
        );
        assert_eq!(excerpt("ååååå", "missing", 4), "ååå…");
    }
}
