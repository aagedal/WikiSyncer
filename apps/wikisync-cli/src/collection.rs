use std::error::Error as StdError;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    InclusionReason, PageTitle, TitleSelection, UnixTimestamp, WikiId,
};
use wikisync_mediawiki::{ClientConfig, MediaWikiClient};
use wikisync_store::{
    CollectionEstimate, Library, NetworkTransferPolicy, ResolvedCollectionMember,
    StoredCollectionConfiguration,
};
use wikisync_sync::{
    CategoryPreviewLimits, CollectionSelectionPreview, parse_title_list, preview_collection_rule,
};
use wikisyncd::{
    CollectionAdministration, CollectionAdministrationOutcome, CollectionDraft,
    CollectionDraftEstimate, MeteredNetworkState, WriterAccess, administer_collection_direct,
    application_user_agent, detect_metered_network,
};

const JSON_SCHEMA_VERSION: u32 = 1;
const MAX_TITLE_LIST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TITLE_LIST_TITLES: usize = 10_000;
const HUMAN_MEMBER_LIMIT: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Add(ChangeOptions),
    Edit {
        collection_id: CollectionId,
        options: ChangeOptions,
    },
    List {
        include_tombstones: bool,
        json: bool,
    },
    Remove {
        collection_id: CollectionId,
        commit: bool,
        json: bool,
    },
    Estimate {
        collection_id: CollectionId,
        json: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChangeOptions {
    wiki_id: Option<WikiId>,
    name: Option<String>,
    scope: Option<Scope>,
    history_policy: Option<HistoryPolicy>,
    maximum_pages: Option<Option<u64>>,
    maximum_bytes: Option<Option<u64>>,
    removal_policy: Option<CollectionRemovalPolicy>,
    commit: bool,
    json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Scope {
    ExplicitTitles(Vec<PageTitle>),
    TitleList(PathBuf),
    Category {
        title: PageTitle,
        recursion_depth: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BudgetAssessment {
    Fits,
    Exceeded,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct Error {
    message: String,
}

impl Error {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

pub(crate) fn parse(values: Vec<String>) -> Result<Command, String> {
    let mut values = values.into_iter();
    match values.next().as_deref() {
        Some("add") => Ok(Command::Add(parse_change_options(values, true)?)),
        Some("edit") => {
            let (collection_id, options) = parse_edit_options(values)?;
            Ok(Command::Edit {
                collection_id,
                options,
            })
        }
        Some("list") => parse_list(values),
        Some("remove") => parse_remove(values),
        Some("estimate") => parse_estimate(values),
        Some(value) => Err(format!("unknown collection subcommand {value:?}")),
        None => Err("collection requires add, edit, list, remove, or estimate".to_owned()),
    }
}

fn parse_edit_options(
    values: impl Iterator<Item = String>,
) -> Result<(CollectionId, ChangeOptions), String> {
    let mut raw = values.collect::<Vec<_>>();
    let mut collection_id = None;
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "--collection" {
            if index + 1 >= raw.len() {
                return Err("--collection requires a value".to_owned());
            }
            let value = parse_collection_id(&raw[index + 1])?;
            if collection_id.replace(value).is_some() {
                return Err("--collection may only be supplied once".to_owned());
            }
            raw.drain(index..=index + 1);
        } else {
            index += 1;
        }
    }
    let collection_id =
        collection_id.ok_or_else(|| "collection edit requires --collection <id>".to_owned())?;
    let options = parse_change_options(raw.into_iter(), false)?;
    if options.wiki_id.is_some() {
        return Err("collection edit cannot change a collection's source wiki".to_owned());
    }
    if options.name.is_none()
        && options.scope.is_none()
        && options.history_policy.is_none()
        && options.maximum_pages.is_none()
        && options.maximum_bytes.is_none()
        && options.removal_policy.is_none()
    {
        return Err("collection edit requires at least one configuration change".to_owned());
    }
    Ok((collection_id, options))
}

fn parse_change_options(
    mut values: impl Iterator<Item = String>,
    add: bool,
) -> Result<ChangeOptions, String> {
    let mut wiki_id = None;
    let mut name = None;
    let mut titles = Vec::new();
    let mut title_list = None;
    let mut category = None;
    let mut recursion_depth = None;
    let mut history_policy = None;
    let mut maximum_pages = None;
    let mut maximum_bytes = None;
    let mut removal_policy = None;
    let mut commit = false;
    let mut dry_run = false;
    let mut json = false;
    while let Some(option) = values.next() {
        let value = |values: &mut dyn Iterator<Item = String>| {
            values
                .next()
                .ok_or_else(|| format!("{option} requires a value"))
        };
        match option.as_str() {
            "--wiki" => set_once(&mut wiki_id, parse_wiki_id(&value(&mut values)?)?, &option)?,
            "--name" => {
                let parsed = value(&mut values)?;
                if parsed.trim().is_empty() {
                    return Err("--name cannot be empty".to_owned());
                }
                set_once(&mut name, parsed, &option)?;
            }
            "--title" => {
                let parsed = PageTitle::new(value(&mut values)?)
                    .map_err(|error| format!("--title requires a valid title: {error}"))?;
                titles.push(parsed);
            }
            "--title-list" => {
                set_once(&mut title_list, PathBuf::from(value(&mut values)?), &option)?;
            }
            "--category" => {
                let parsed = PageTitle::new(value(&mut values)?)
                    .map_err(|error| format!("--category requires a valid title: {error}"))?;
                if !parsed.as_str().starts_with("Category:") {
                    return Err("--category requires a fully qualified Category:title".to_owned());
                }
                set_once(&mut category, parsed, &option)?;
            }
            "--depth" => {
                let parsed = value(&mut values)?
                    .parse::<u16>()
                    .map_err(|_| "--depth requires an integer from 0 to 65535".to_owned())?;
                set_once(&mut recursion_depth, parsed, &option)?;
            }
            "--history" => {
                let parsed = parse_history_policy(&value(&mut values)?)?;
                set_once(&mut history_policy, parsed, &option)?;
            }
            "--max-pages" => {
                let parsed = parse_optional_limit(&value(&mut values)?, &option)?;
                set_once(&mut maximum_pages, parsed, &option)?;
            }
            "--max-bytes" => {
                let parsed = parse_optional_limit(&value(&mut values)?, &option)?;
                set_once(&mut maximum_bytes, parsed, &option)?;
            }
            "--removal-policy" => {
                let parsed = match value(&mut values)?.as_str() {
                    "retain-history" | "stop-tracking-retain-history" => {
                        CollectionRemovalPolicy::StopTrackingRetainHistory
                    }
                    "keep-tracking" => CollectionRemovalPolicy::KeepTracking,
                    _ => {
                        return Err(
                            "--removal-policy must be retain-history or keep-tracking".to_owned()
                        );
                    }
                };
                set_once(&mut removal_policy, parsed, &option)?;
            }
            "--commit" => commit = true,
            "--dry-run" => dry_run = true,
            "--json" => json = true,
            _ => return Err(format!("unknown collection option {option:?}")),
        }
    }
    if commit && dry_run {
        return Err("--commit and --dry-run cannot be used together".to_owned());
    }
    if recursion_depth.is_some() && category.is_none() {
        return Err("--depth is only valid with --category".to_owned());
    }
    let scope_count = usize::from(!titles.is_empty())
        + usize::from(title_list.is_some())
        + usize::from(category.is_some());
    if scope_count > 1 {
        return Err(
            "collection scope must use exactly one of --title, --title-list, or --category"
                .to_owned(),
        );
    }
    let scope = if !titles.is_empty() {
        Some(Scope::ExplicitTitles(titles))
    } else if let Some(path) = title_list {
        Some(Scope::TitleList(path))
    } else {
        category.map(|title| Scope::Category {
            title,
            recursion_depth: recursion_depth.unwrap_or(0),
        })
    };
    if add {
        if wiki_id.is_none() {
            return Err("collection add requires --wiki <id>".to_owned());
        }
        if name.is_none() {
            return Err("collection add requires --name <name>".to_owned());
        }
        if scope.is_none() {
            return Err("collection add requires --title, --title-list, or --category".to_owned());
        }
    }
    Ok(ChangeOptions {
        wiki_id,
        name,
        scope,
        history_policy,
        maximum_pages,
        maximum_bytes,
        removal_policy,
        commit,
        json,
    })
}

fn parse_list(mut values: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut include_tombstones = false;
    let mut json = false;
    for option in &mut values {
        match option.as_str() {
            "--all" => include_tombstones = true,
            "--json" => json = true,
            _ => return Err(format!("unknown collection list option {option:?}")),
        }
    }
    Ok(Command::List {
        include_tombstones,
        json,
    })
}

fn parse_remove(mut values: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut collection_id = None;
    let mut commit = false;
    let mut dry_run = false;
    let mut json = false;
    while let Some(option) = values.next() {
        match option.as_str() {
            "--collection" => {
                let raw = values
                    .next()
                    .ok_or_else(|| "--collection requires a value".to_owned())?;
                set_once(&mut collection_id, parse_collection_id(&raw)?, &option)?;
            }
            "--commit" => commit = true,
            "--dry-run" => dry_run = true,
            "--json" => json = true,
            _ => return Err(format!("unknown collection remove option {option:?}")),
        }
    }
    if commit && dry_run {
        return Err("--commit and --dry-run cannot be used together".to_owned());
    }
    Ok(Command::Remove {
        collection_id: collection_id
            .ok_or_else(|| "collection remove requires --collection <id>".to_owned())?,
        commit,
        json,
    })
}

fn parse_estimate(mut values: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut collection_id = None;
    let mut json = false;
    while let Some(option) = values.next() {
        match option.as_str() {
            "--collection" => {
                let raw = values
                    .next()
                    .ok_or_else(|| "--collection requires a value".to_owned())?;
                set_once(&mut collection_id, parse_collection_id(&raw)?, &option)?;
            }
            "--json" => json = true,
            _ => return Err(format!("unknown collection estimate option {option:?}")),
        }
    }
    Ok(Command::Estimate {
        collection_id: collection_id
            .ok_or_else(|| "collection estimate requires --collection <id>".to_owned())?,
        json,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("{option} may only be supplied once"));
    }
    Ok(())
}

fn parse_wiki_id(value: &str) -> Result<WikiId, String> {
    let raw = value
        .parse::<u64>()
        .map_err(|_| "--wiki requires a positive integer".to_owned())?;
    WikiId::new(raw).map_err(|error| error.to_string())
}

fn parse_collection_id(value: &str) -> Result<CollectionId, String> {
    let raw = value
        .parse::<u64>()
        .map_err(|_| "--collection requires a positive integer".to_owned())?;
    CollectionId::new(raw).map_err(|error| error.to_string())
}

fn parse_optional_limit(value: &str, option: &str) -> Result<Option<u64>, String> {
    if value == "unlimited" {
        return Ok(None);
    }
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("{option} requires a positive integer or unlimited"))?;
    if value == 0 {
        return Err(format!("{option} requires a positive integer or unlimited"));
    }
    Ok(Some(value))
}

fn parse_history_policy(value: &str) -> Result<HistoryPolicy, String> {
    if value == "current-and-future" {
        return Ok(HistoryPolicy::CurrentAndFuture);
    }
    if value == "complete" {
        return Ok(HistoryPolicy::Complete);
    }
    if let Some(count) = value.strip_prefix("last-n:") {
        let count = count
            .parse::<u32>()
            .map_err(|_| "last-n history requires last-n:COUNT".to_owned())?;
        return HistoryPolicy::last_n(count).map_err(|error| error.to_string());
    }
    if let Some(seconds) = value.strip_prefix("since:") {
        let seconds = seconds
            .parse::<i64>()
            .map_err(|_| "since history requires since:UNIX-SECONDS".to_owned())?;
        return Ok(HistoryPolicy::Since(UnixTimestamp::from_seconds(seconds)));
    }
    Err(
        "--history must be current-and-future, last-n:COUNT, since:UNIX-SECONDS, or complete"
            .to_owned(),
    )
}

pub(crate) fn run(library_root: &Path, command: Command) -> Result<(), Error> {
    match command {
        Command::Add(options) => add(library_root, options),
        Command::Edit {
            collection_id,
            options,
        } => edit(library_root, collection_id, options),
        Command::List {
            include_tombstones,
            json,
        } => list(library_root, include_tombstones, json),
        Command::Remove {
            collection_id,
            commit,
            json,
        } => remove(library_root, collection_id, commit, json),
        Command::Estimate {
            collection_id,
            json,
        } => estimate(library_root, collection_id, json),
    }
}

fn add(library_root: &Path, options: ChangeOptions) -> Result<(), Error> {
    let library = Library::open_read_only(library_root).map_err(display_error)?;
    let wiki_id = options.wiki_id.expect("validated add wiki");
    let name = options.name.clone().expect("validated add name");
    let rule = scope_rule(options.scope.as_ref().expect("validated add scope"))?;
    let history_policy = options
        .history_policy
        .unwrap_or(HistoryPolicy::CurrentAndFuture);
    let budget = build_budget(
        CollectionBudget::unlimited(),
        options.maximum_pages,
        options.maximum_bytes,
    )?;
    let removal_policy = options
        .removal_policy
        .unwrap_or(CollectionRemovalPolicy::StopTrackingRetainHistory);
    let preview = preview_rule(&library, wiki_id, &rule)?;
    let draft = CollectionDraft {
        wiki_id,
        name,
        preview,
        history_policy,
        budget,
        removal_policy,
    };
    let budget_assessment = preview_budget_assessment(&draft, 0);
    if options.commit && budget_assessment == BudgetAssessment::Exceeded {
        return Err(Error::message(
            "collection preview exceeds a configured hard budget; no changes were committed",
        ));
    }
    if options.commit {
        let outcome = administer(library_root, CollectionAdministration::Add(draft.clone()))?;
        print_change(
            "add",
            true,
            &draft,
            budget_assessment,
            Some(&outcome),
            options.json,
        )
    } else {
        print_change("add", false, &draft, budget_assessment, None, options.json)
    }
}

fn edit(
    library_root: &Path,
    collection_id: CollectionId,
    options: ChangeOptions,
) -> Result<(), Error> {
    let library = Library::open_read_only(library_root).map_err(display_error)?;
    let configuration = configured_collection(&library, collection_id)?;
    let current_estimate = library
        .collection_estimate(collection_id)
        .map_err(display_error)?;
    let rule = options
        .scope
        .as_ref()
        .map(scope_rule)
        .transpose()?
        .unwrap_or_else(|| configuration.rule.clone());
    let preview = if options.scope.is_some() {
        preview_rule(&library, configuration.wiki_id, &rule)?
    } else {
        existing_preview(&library, &configuration)?
    };
    let draft = CollectionDraft {
        wiki_id: configuration.wiki_id,
        name: options.name.unwrap_or(configuration.name),
        preview,
        history_policy: options
            .history_policy
            .unwrap_or(configuration.history_policy),
        budget: build_budget(
            configuration.budget,
            options.maximum_pages,
            options.maximum_bytes,
        )?,
        removal_policy: options
            .removal_policy
            .unwrap_or(configuration.removal_policy),
    };
    let budget_assessment =
        preview_budget_assessment(&draft, current_estimate.current_canonical_bytes);
    if options.commit && budget_assessment == BudgetAssessment::Exceeded {
        return Err(Error::message(
            "collection preview exceeds a configured hard budget; no changes were committed",
        ));
    }
    if options.commit {
        let outcome = administer(
            library_root,
            CollectionAdministration::Edit {
                collection_id,
                expected_generation: configuration.generation,
                draft: draft.clone(),
            },
        )?;
        print_change(
            "edit",
            true,
            &draft,
            budget_assessment,
            Some(&outcome),
            options.json,
        )
    } else {
        print_change("edit", false, &draft, budget_assessment, None, options.json)
    }
}

fn estimate(
    library_root: &Path,
    collection_id: CollectionId,
    json_output: bool,
) -> Result<(), Error> {
    let library = Library::open_read_only(library_root).map_err(display_error)?;
    let configuration = configured_collection(&library, collection_id)?;
    let current_estimate = library
        .collection_estimate(collection_id)
        .map_err(display_error)?;
    let preview = preview_rule(&library, configuration.wiki_id, &configuration.rule)?;
    let draft = CollectionDraft {
        wiki_id: configuration.wiki_id,
        name: configuration.name,
        preview,
        history_policy: configuration.history_policy,
        budget: configuration.budget,
        removal_policy: configuration.removal_policy,
    };
    print_change(
        "estimate",
        false,
        &draft,
        preview_budget_assessment(&draft, current_estimate.current_canonical_bytes),
        None,
        json_output,
    )
}

fn list(library_root: &Path, include_tombstones: bool, json_output: bool) -> Result<(), Error> {
    let library = Library::open_read_only(library_root).map_err(display_error)?;
    let collections = if include_tombstones {
        library
            .collections_including_tombstones()
            .map_err(display_error)?
    } else {
        library.collections().map_err(display_error)?
    };
    let mut values = Vec::with_capacity(collections.len());
    for collection in &collections {
        let configuration = library
            .collection_configuration(collection.collection_id)
            .map_err(display_error)?;
        let estimate = library
            .collection_estimate(collection.collection_id)
            .map_err(display_error)?;
        values.push(json!({
            "collection_id": collection.collection_id.get(),
            "wiki_id": collection.wiki_id.get(),
            "name": collection.name,
            "generation": collection.generation,
            "status": collection.status.as_str(),
            "page_count": collection.page_count,
            "configuration": configuration.as_ref().map(configuration_json),
            "estimate": estimate_json(estimate),
        }));
    }
    if json_output {
        write_json(&json!({
            "schema_version": JSON_SCHEMA_VERSION,
            "includes_tombstones": include_tombstones,
            "collections": values,
        }))?;
    } else if collections.is_empty() {
        println!("No collections configured.");
    } else {
        for (collection, value) in collections.iter().zip(values) {
            println!(
                "{}\twiki {}\t{}\t{} pages\t{}",
                collection.collection_id,
                collection.wiki_id,
                value["status"].as_str().unwrap_or("unknown"),
                collection.page_count,
                collection.name,
            );
        }
    }
    Ok(())
}

fn remove(
    library_root: &Path,
    collection_id: CollectionId,
    commit: bool,
    json_output: bool,
) -> Result<(), Error> {
    let library = Library::open_read_only(library_root).map_err(display_error)?;
    let collection = library
        .collection(collection_id)
        .map_err(display_error)?
        .ok_or_else(|| Error::message(format!("collection {collection_id} was not found")))?;
    let before = json!({
        "collection_id": collection.collection_id.get(),
        "wiki_id": collection.wiki_id.get(),
        "name": collection.name,
        "generation": collection.generation,
        "status": collection.status.as_str(),
        "page_count": collection.page_count,
    });
    let outcome = if commit {
        Some(administer(
            library_root,
            CollectionAdministration::Remove { collection_id },
        )?)
    } else {
        None
    };
    if json_output {
        write_json(&json!({
            "schema_version": JSON_SCHEMA_VERSION,
            "operation": "remove",
            "committed": commit,
            "collection": before,
            "result": outcome.as_ref().map(outcome_json),
            "effect": "stop-tracking-retain-history",
        }))?;
    } else if commit {
        println!(
            "Stopped tracking collection {} ({}); retained captured history and integrity evidence.",
            collection_id, collection.name
        );
    } else {
        println!(
            "Preview only: collection {} ({}, {} active pages) would stop tracking; captured history and integrity evidence would be retained.",
            collection_id, collection.name, collection.page_count
        );
        println!("Run again with --commit to apply this non-destructive removal.");
    }
    Ok(())
}

fn configured_collection(
    library: &Library,
    collection_id: CollectionId,
) -> Result<StoredCollectionConfiguration, Error> {
    let collection = library
        .collection(collection_id)
        .map_err(display_error)?
        .ok_or_else(|| Error::message(format!("collection {collection_id} was not found")))?;
    if collection.status.as_str() != "active" {
        return Err(Error::message(format!(
            "collection {collection_id} is tombstoned and cannot be changed or refreshed"
        )));
    }
    library
        .collection_configuration(collection_id)
        .map_err(display_error)?
        .ok_or_else(|| Error::message(format!("collection {collection_id} is not configured")))
}

fn existing_preview(
    library: &Library,
    configuration: &StoredCollectionConfiguration,
) -> Result<CollectionSelectionPreview, Error> {
    let estimate = library
        .collection_estimate(configuration.collection_id)
        .map_err(display_error)?;
    Ok(CollectionSelectionPreview {
        rule: configuration.rule.clone(),
        members: library
            .resolved_collection_members(configuration.collection_id)
            .map_err(display_error)?,
        missing_titles: library
            .unresolved_titles(configuration.collection_id)
            .map_err(display_error)?,
        predicted_canonical_bytes: estimate.predicted_canonical_bytes,
        category_batches: 0,
    })
}

fn scope_rule(scope: &Scope) -> Result<CollectionRule, Error> {
    match scope {
        Scope::ExplicitTitles(titles) => TitleSelection::new(titles.clone())
            .map(CollectionRule::ExplicitTitles)
            .map_err(display_error),
        Scope::TitleList(path) => {
            let mut file = File::open(path).map_err(|error| {
                Error::message(format!(
                    "cannot open title list {}: {error}",
                    path.display()
                ))
            })?;
            let metadata = file.metadata().map_err(|error| {
                Error::message(format!(
                    "cannot inspect opened title list {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_file() {
                return Err(Error::message(format!(
                    "title list {} is not a regular file",
                    path.display()
                )));
            }
            let source = read_bounded_utf8(&mut file, path)?;
            parse_title_list(&source, MAX_TITLE_LIST_TITLES)
                .map(CollectionRule::TitleList)
                .map_err(display_error)
        }
        Scope::Category {
            title,
            recursion_depth,
        } => Ok(CollectionRule::Category {
            title: title.clone(),
            recursion_depth: *recursion_depth,
        }),
    }
}

fn read_bounded_utf8(reader: &mut impl Read, path: &Path) -> Result<String, Error> {
    let maximum_plus_one = MAX_TITLE_LIST_BYTES
        .checked_add(1)
        .expect("title-list byte limit can be incremented");
    let mut bytes = Vec::new();
    reader
        .take(maximum_plus_one)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Error::message(format!(
                "cannot read title list {}: {error}",
                path.display()
            ))
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TITLE_LIST_BYTES {
        return Err(Error::message(format!(
            "title list {} exceeds the {}-byte limit",
            path.display(),
            MAX_TITLE_LIST_BYTES
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| Error::message(format!("title list {} is not valid UTF-8", path.display())))
}

fn build_budget(
    current: CollectionBudget,
    maximum_pages: Option<Option<u64>>,
    maximum_bytes: Option<Option<u64>>,
) -> Result<CollectionBudget, Error> {
    let pages = maximum_pages.unwrap_or_else(|| current.maximum_pages().map(|value| value.get()));
    let bytes = maximum_bytes.unwrap_or_else(|| current.maximum_bytes().map(|value| value.get()));
    let mut budget = CollectionBudget::unlimited();
    if let Some(pages) = pages {
        budget = budget.with_maximum_pages(pages).map_err(display_error)?;
    }
    if let Some(bytes) = bytes {
        budget = budget.with_maximum_bytes(bytes).map_err(display_error)?;
    }
    Ok(budget)
}

fn preview_rule(
    library: &Library,
    wiki_id: WikiId,
    rule: &CollectionRule,
) -> Result<CollectionSelectionPreview, Error> {
    let wiki = library
        .wiki(wiki_id)
        .map_err(display_error)?
        .ok_or_else(|| Error::message(format!("source wiki {wiki_id} was not found")))?;
    let policy = library.network_transfer_policy().map_err(display_error)?;
    if policy.avoid_metered_networks()
        && detect_metered_network().state == MeteredNetworkState::Metered
    {
        return Err(Error::message(
            "collection preview is blocked by the library policy while the active network is metered",
        ));
    }
    let config = configured_client(&wiki.api_endpoint, policy)?;
    let client = MediaWikiClient::new(config).map_err(display_error)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(display_error)?;
    runtime
        .block_on(preview_collection_rule(
            &client,
            rule,
            CategoryPreviewLimits::default(),
        ))
        .map_err(display_error)
}

fn configured_client(endpoint: &str, policy: NetworkTransferPolicy) -> Result<ClientConfig, Error> {
    let concurrent = usize::try_from(policy.max_concurrent_requests())
        .map_err(|_| Error::message("maximum concurrent request policy is too large"))?;
    let rate = policy
        .max_download_bytes_per_second()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| Error::message("maximum byte-rate policy is too large"))?;
    ClientConfig::new(endpoint, application_user_agent().map_err(display_error)?)
        .and_then(|config| config.with_max_concurrent_requests(concurrent))
        .and_then(|config| config.with_max_downloaded_response_bytes_per_second(rate))
        .map_err(display_error)
}

fn administer(
    library_root: &Path,
    request: CollectionAdministration,
) -> Result<CollectionAdministrationOutcome, Error> {
    match WriterAccess::discover(library_root).map_err(display_error)? {
        WriterAccess::Daemon(client) => client
            .administer_collection(request)
            .map_err(collection_admin_error),
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(library_root).map_err(display_error)?;
            administer_collection_direct(&mut library, request).map_err(collection_admin_error)
        }
    }
}

fn collection_admin_error(error: impl fmt::Display) -> Error {
    let message = error.to_string();
    if message.contains("changed while it was being previewed") {
        Error::message(format!(
            "{message}; re-run collection edit to load the latest collection generation and re-preview"
        ))
    } else {
        Error::message(message)
    }
}

fn preview_budget_assessment(
    draft: &CollectionDraft,
    current_canonical_bytes: u64,
) -> BudgetAssessment {
    let pages = u64::try_from(draft.preview.members.len()).unwrap_or(u64::MAX);
    if draft
        .budget
        .maximum_pages()
        .is_some_and(|maximum| pages > maximum.get())
    {
        return BudgetAssessment::Exceeded;
    }
    let Some(maximum_bytes) = draft.budget.maximum_bytes() else {
        return BudgetAssessment::Fits;
    };
    match draft.preview.predicted_canonical_bytes {
        Some(predicted) if predicted.max(current_canonical_bytes) > maximum_bytes.get() => {
            BudgetAssessment::Exceeded
        }
        Some(_) => BudgetAssessment::Fits,
        None if current_canonical_bytes > maximum_bytes.get() => BudgetAssessment::Exceeded,
        None => BudgetAssessment::Unknown,
    }
}

fn print_change(
    operation: &str,
    committed: bool,
    draft: &CollectionDraft,
    budget_assessment: BudgetAssessment,
    outcome: Option<&CollectionAdministrationOutcome>,
    json_output: bool,
) -> Result<(), Error> {
    if json_output {
        write_json(&json!({
            "schema_version": JSON_SCHEMA_VERSION,
            "operation": operation,
            "committed": committed,
            "configuration": draft_json(draft),
            "preview": preview_json(&draft.preview, budget_assessment),
            "result": outcome.map(outcome_json),
        }))?;
        return Ok(());
    }
    println!(
        "{} {}: {} resolved page{}, {} missing title{}, predicted canonical bytes {}.",
        if committed {
            "Committed"
        } else {
            "Preview only for"
        },
        operation,
        draft.preview.members.len(),
        if draft.preview.members.len() == 1 {
            ""
        } else {
            "s"
        },
        draft.preview.missing_titles.len(),
        if draft.preview.missing_titles.len() == 1 {
            ""
        } else {
            "s"
        },
        draft
            .preview
            .predicted_canonical_bytes
            .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
    );
    println!(
        "Hard budgets: {}.",
        match budget_assessment {
            BudgetAssessment::Fits => "fit",
            BudgetAssessment::Exceeded => "exceeded",
            BudgetAssessment::Unknown => {
                "unknown because the source did not provide a complete byte estimate"
            }
        }
    );
    for member in draft.preview.members.iter().take(HUMAN_MEMBER_LIMIT) {
        println!("{}\t{}", member.page_id, member.title);
    }
    if draft.preview.members.len() > HUMAN_MEMBER_LIMIT {
        println!(
            "... {} additional resolved pages omitted from human output; use --json for the complete stable preview.",
            draft.preview.members.len() - HUMAN_MEMBER_LIMIT
        );
    }
    if !committed {
        println!("Run again with --commit to apply this preview.");
    }
    Ok(())
}

fn write_json(value: &Value) -> Result<(), Error> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, value).map_err(display_error)?;
    output.write_all(b"\n").map_err(display_error)
}

fn draft_json(draft: &CollectionDraft) -> Value {
    json!({
        "wiki_id": draft.wiki_id.get(),
        "name": draft.name,
        "rule": rule_json(&draft.preview.rule),
        "history_policy": history_json(draft.history_policy),
        "budget": budget_json(draft.budget),
        "removal_policy": removal_policy_name(draft.removal_policy),
    })
}

fn configuration_json(configuration: &StoredCollectionConfiguration) -> Value {
    json!({
        "wiki_id": configuration.wiki_id.get(),
        "name": configuration.name,
        "generation": configuration.generation,
        "rule": rule_json(&configuration.rule),
        "history_policy": history_json(configuration.history_policy),
        "budget": budget_json(configuration.budget),
        "removal_policy": removal_policy_name(configuration.removal_policy),
        "status": configuration.status.as_str(),
    })
}

fn rule_json(rule: &CollectionRule) -> Value {
    match rule {
        CollectionRule::WholeMainNamespace => json!({
            "kind": "whole-main-namespace",
        }),
        CollectionRule::ExplicitTitles(titles) => json!({
            "kind": "explicit-titles",
            "titles": titles.iter().map(PageTitle::as_str).collect::<Vec<_>>(),
        }),
        CollectionRule::TitleList(titles) => json!({
            "kind": "title-list",
            "titles": titles.iter().map(PageTitle::as_str).collect::<Vec<_>>(),
        }),
        CollectionRule::Category {
            title,
            recursion_depth,
        } => json!({
            "kind": "category",
            "title": title.as_str(),
            "recursion_depth": recursion_depth,
        }),
    }
}

fn history_json(policy: HistoryPolicy) -> Value {
    match policy {
        HistoryPolicy::CurrentAndFuture => json!({"kind": "current-and-future"}),
        HistoryPolicy::LastN(count) => json!({"kind": "last-n", "count": count.get()}),
        HistoryPolicy::Since(timestamp) => {
            json!({"kind": "since", "unix_seconds": timestamp.as_seconds()})
        }
        HistoryPolicy::Complete => json!({"kind": "complete"}),
    }
}

fn budget_json(budget: CollectionBudget) -> Value {
    json!({
        "maximum_pages": budget.maximum_pages().map(|value| value.get()),
        "maximum_canonical_bytes": budget.maximum_bytes().map(|value| value.get()),
    })
}

fn preview_json(
    preview: &CollectionSelectionPreview,
    budget_assessment: BudgetAssessment,
) -> Value {
    let limits = CategoryPreviewLimits::default();
    json!({
        "complete": true,
        "fits_budget": match budget_assessment {
            BudgetAssessment::Fits => Some(true),
            BudgetAssessment::Exceeded => Some(false),
            BudgetAssessment::Unknown => None,
        },
        "budget_assessment": match budget_assessment {
            BudgetAssessment::Fits => "fits",
            BudgetAssessment::Exceeded => "exceeded",
            BudgetAssessment::Unknown => "unknown",
        },
        "resolved_page_count": preview.members.len(),
        "missing_title_count": preview.missing_titles.len(),
        "predicted_canonical_bytes": preview.predicted_canonical_bytes,
        "category_batches": preview.category_batches,
        "limits": {
            "maximum_titles": MAX_TITLE_LIST_TITLES,
            "maximum_category_depth": limits.max_recursion_depth,
            "maximum_categories": limits.max_categories,
            "maximum_pages": limits.max_pages,
            "maximum_api_responses": limits.max_batches,
        },
        "members": preview.members.iter().map(member_json).collect::<Vec<_>>(),
        "missing_titles": preview.missing_titles.iter().map(PageTitle::as_str).collect::<Vec<_>>(),
    })
}

fn member_json(member: &ResolvedCollectionMember) -> Value {
    json!({
        "page_id": member.page_id.get(),
        "namespace": member.namespace,
        "title": member.title.as_str(),
        "inclusion_reason": inclusion_reason_json(&member.inclusion_reason),
    })
}

fn inclusion_reason_json(reason: &InclusionReason) -> Value {
    match reason {
        InclusionReason::WholeMainNamespace => {
            json!({"kind": "whole-main-namespace"})
        }
        InclusionReason::ExplicitTitle(title) => {
            json!({"kind": "explicit-title", "title": title.as_str()})
        }
        InclusionReason::TitleList(title) => {
            json!({"kind": "title-list", "title": title.as_str()})
        }
        InclusionReason::Category { category, depth } => json!({
            "kind": "category",
            "category": category.as_str(),
            "depth": depth,
        }),
    }
}

fn estimate_json(estimate: CollectionEstimate) -> Value {
    json!({
        "resolved_page_count": estimate.resolved_page_count,
        "current_canonical_bytes": estimate.current_canonical_bytes,
        "predicted_canonical_bytes": estimate.predicted_canonical_bytes,
        "expected_canonical_bytes": estimate.expected_canonical_bytes(),
        "predicted_at": estimate.predicted_at,
    })
}

fn removal_policy_name(policy: CollectionRemovalPolicy) -> &'static str {
    match policy {
        CollectionRemovalPolicy::StopTrackingRetainHistory => "stop-tracking-retain-history",
        CollectionRemovalPolicy::KeepTracking => "keep-tracking",
    }
}

fn outcome_json(outcome: &CollectionAdministrationOutcome) -> Value {
    match outcome {
        CollectionAdministrationOutcome::Estimated(estimate) => json!({
            "kind": "estimate",
            "estimate": draft_estimate_json(*estimate),
        }),
        CollectionAdministrationOutcome::Added {
            collection_id,
            estimate,
        } => json!({
            "kind": "added",
            "collection_id": collection_id.get(),
            "estimate": draft_estimate_json(*estimate),
        }),
        CollectionAdministrationOutcome::Edited {
            collection_id,
            estimate,
        } => json!({
            "kind": "edited",
            "collection_id": collection_id.get(),
            "estimate": draft_estimate_json(*estimate),
        }),
        CollectionAdministrationOutcome::Removed { collection_id } => json!({
            "kind": "removed",
            "collection_id": collection_id.get(),
        }),
    }
}

fn draft_estimate_json(estimate: CollectionDraftEstimate) -> Value {
    json!({
        "resolved_page_count": estimate.resolved_page_count,
        "missing_title_count": estimate.missing_title_count,
        "predicted_canonical_bytes": estimate.predicted_canonical_bytes,
        "category_batches": estimate.category_batches,
        "fits_budget": estimate.fits_budget,
    })
}

fn display_error(error: impl fmt::Display) -> Error {
    Error::message(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct GrowingReader {
        remaining: u64,
        bytes_read: u64,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = usize::try_from(self.remaining.min(buffer.len() as u64))
                .expect("bounded read count");
            buffer[..count].fill(b'x');
            self.remaining -= count as u64;
            self.bytes_read += count as u64;
            Ok(count)
        }
    }

    #[test]
    fn parses_bounded_add_preview_and_commit_options() {
        assert_eq!(
            parse(vec![
                "add".to_owned(),
                "--wiki".to_owned(),
                "2".to_owned(),
                "--name".to_owned(),
                "Reference".to_owned(),
                "--title".to_owned(),
                "Rust".to_owned(),
                "--title".to_owned(),
                "Cargo".to_owned(),
                "--history".to_owned(),
                "last-n:5".to_owned(),
                "--max-pages".to_owned(),
                "100".to_owned(),
                "--commit".to_owned(),
                "--json".to_owned(),
            ])
            .expect("add"),
            Command::Add(ChangeOptions {
                wiki_id: Some(WikiId::new(2).expect("wiki")),
                name: Some("Reference".to_owned()),
                scope: Some(Scope::ExplicitTitles(vec![
                    PageTitle::new("Rust").expect("title"),
                    PageTitle::new("Cargo").expect("title"),
                ])),
                history_policy: Some(HistoryPolicy::last_n(5).expect("history")),
                maximum_pages: Some(Some(100)),
                maximum_bytes: None,
                removal_policy: None,
                commit: true,
                json: true,
            })
        );
    }

    #[test]
    fn rejects_ambiguous_scope_and_unconfirmed_flag_conflict() {
        let ambiguous = parse(vec![
            "add".to_owned(),
            "--wiki".to_owned(),
            "1".to_owned(),
            "--name".to_owned(),
            "bad".to_owned(),
            "--title".to_owned(),
            "Rust".to_owned(),
            "--category".to_owned(),
            "Category:Rust".to_owned(),
        ])
        .expect_err("ambiguous");
        assert!(ambiguous.contains("exactly one"));

        let conflict = parse(vec![
            "remove".to_owned(),
            "--collection".to_owned(),
            "1".to_owned(),
            "--commit".to_owned(),
            "--dry-run".to_owned(),
        ])
        .expect_err("conflict");
        assert!(conflict.contains("cannot be used together"));
    }

    #[test]
    fn growing_title_list_is_read_only_through_the_limit_plus_one() {
        let mut reader = GrowingReader {
            remaining: MAX_TITLE_LIST_BYTES + 16 * 1024,
            bytes_read: 0,
        };
        let error = read_bounded_utf8(&mut reader, Path::new("fixture-titles.txt"))
            .expect_err("oversized stream");
        assert!(error.to_string().contains("exceeds the 2097152-byte limit"));
        assert_eq!(reader.bytes_read, MAX_TITLE_LIST_BYTES + 1);
        assert_eq!(reader.remaining, 16 * 1024 - 1);
    }

    #[test]
    fn bounded_title_list_rejects_invalid_utf8_without_echoing_bytes() {
        let mut source = io::Cursor::new(vec![b'R', b'u', b's', b't', b'\n', 0xff]);
        let error = read_bounded_utf8(&mut source, Path::new("fixture-titles.txt"))
            .expect_err("invalid UTF-8");
        assert_eq!(
            error.to_string(),
            "title list fixture-titles.txt is not valid UTF-8"
        );
    }
}
