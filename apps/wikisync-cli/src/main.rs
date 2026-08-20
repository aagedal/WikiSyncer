use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str;

use serde_json::json;
use wikisync_content::{to_markdown, to_plain_text};
use wikisync_core::{PageTitle, WikiId};
use wikisync_search::{
    MAX_SEARCH_RESULTS, SearchError, SearchIndex, SearchQuery, SqliteSearchIndex,
};
use wikisync_store::{Library, StoreError, StoredPage, StoredRevision};

const USAGE: &str = "WikiSyncer offline reader

Usage:
  wikisync --library <path> search [--wiki <id>] [--limit <count>] [--json] <query>
  wikisync --library <path> show [--wiki <id>] [--json] [--source] <title>
  wikisync --help
  wikisync --version

The WIKISYNC_LIBRARY environment variable may replace --library.";

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
        Action::Command { library, command } => {
            if !library.join("library.sqlite3").is_file() {
                return Err(CliError::message(format!(
                    "{} is not an initialized WikiSyncer library",
                    library.display()
                )));
            }
            let library = Library::open(library)?;
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
                    json,
                    source,
                } => show(&library, &title, wiki_id, json, source),
            }
        }
    }
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
    json_output: bool,
    exact_source: bool,
) -> Result<(), CliError> {
    let matches = library.pages_by_title(title, wiki_id)?;
    let page = unique_page(matches, title, wiki_id)?;
    let revision_id = page
        .current_revision_id
        .ok_or_else(|| CliError::data("captured page has no current revision"))?;
    let revision = library
        .revision(page.wiki_id, revision_id)?
        .ok_or_else(|| CliError::data("page head points to a missing revision"))?;
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

fn canonical_source(library: &Library, revision: &StoredRevision) -> Result<String, CliError> {
    let bytes = library.read_object(revision.content_object_id)?;
    String::from_utf8(bytes)
        .map_err(|_| CliError::data("canonical wikitext object is not valid UTF-8"))
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
    Command { library: PathBuf, command: Command },
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
        json: bool,
        source: bool,
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
            Some("search" | "show") => break argument,
            Some(value) => return Err(CliError::usage(format!("unknown command {value:?}"))),
            None => return Err(CliError::usage("arguments must be valid UTF-8")),
        }
    };
    let library = library.ok_or_else(|| {
        CliError::usage("--library <path> or WIKISYNC_LIBRARY is required for offline commands")
    })?;
    let values = arguments
        .map(|value| {
            value
                .into_string()
                .map_err(|_| CliError::usage("arguments must be valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = match command.to_str() {
        Some("search") => parse_search(values)?,
        Some("show") => parse_show(values)?,
        _ => unreachable!("validated command"),
    };
    Ok(Action::Command { library, command })
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
    let mut json = false;
    let mut source = false;
    let mut title = Vec::new();
    let mut values = values.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--wiki" => wiki_id = Some(parse_wiki(required_value(&mut values, "--wiki")?)?),
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
        json,
        source,
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
    fn excerpt_prefers_a_matching_line_and_bounds_characters() {
        assert_eq!(
            excerpt("Heading\nA memory-safe systems language.\n", "memory", 80),
            "A memory-safe systems language."
        );
        assert_eq!(excerpt("ååååå", "missing", 4), "ååå…");
    }
}
