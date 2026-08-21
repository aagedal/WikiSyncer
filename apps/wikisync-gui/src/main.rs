use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::widget::{
    Space, button, checkbox, column, container, horizontal_rule, progress_bar, row, scrollable,
    text, text_input,
};
use iced::{Alignment, Element, Length, Task, Theme};
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    PageTitle, UnixTimestamp,
};
use wikisync_integrity::{VerificationReport, VerificationScope, verify_library};
use wikisync_mediawiki::ClientConfig;
use wikisync_store::{Library, StoredCollection, SyncCheckpoint, SyncRunState, SyncRunStatus};
use wikisync_sync::{
    CategoryPreviewLimits, CollectionSelectionPreview, bootstrap_collection,
    commit_collection_preview, parse_title_list, preview_collection_rule,
    reconcile_collection_heads,
};
use wikisync_web::ReaderHandle;

const DATABASE_NAME: &str = "library.sqlite3";
const RECENT_REVISION_LIMIT: u32 = 12;

fn main() -> iced::Result {
    iced::application("WikiSyncer", App::update, App::view)
        .theme(|_| Theme::Dark)
        .window_size((1120.0, 760.0))
        .run_with(App::new)
}

#[derive(Debug)]
struct App {
    library_path: String,
    privacy_acknowledged: bool,
    screen: Screen,
    tab: Tab,
    snapshot: Option<DashboardSnapshot>,
    notice: Option<Notice>,
    active_request: Option<RequestKey>,
    next_request_id: u64,
    latest_probe_id: u64,
    path_status: PathStatus,
    collection_form: CollectionForm,
    selection_preview: Option<CollectionSelectionPreview>,
    verification: VerificationState,
    reader: Option<Arc<ReaderHandle>>,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let library_path = suggested_library_path();
        let probe_key = RequestKey {
            id: 1,
            path: PathBuf::from(&library_path),
        };
        let app = Self {
            library_path,
            privacy_acknowledged: false,
            screen: Screen::Setup,
            tab: Tab::Overview,
            snapshot: None,
            notice: None,
            active_request: None,
            next_request_id: 2,
            latest_probe_id: probe_key.id,
            path_status: PathStatus::Checking,
            collection_form: CollectionForm::default(),
            selection_preview: None,
            verification: VerificationState::NotRun,
            reader: None,
        };
        (app, probe_task(probe_key))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::LibraryPathChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.library_path = value;
                self.path_status = PathStatus::Checking;
                let key = self.next_key(PathBuf::from(self.library_path.trim()));
                self.latest_probe_id = key.id;
                return probe_task(key);
            }
            Message::PathProbed(completion) => {
                if completion.key.id != self.latest_probe_id
                    || completion.key.path.as_path() != Path::new(self.library_path.trim())
                {
                    return Task::none();
                }
                self.path_status = match completion.result {
                    Ok(true) => PathStatus::ExistingLibrary,
                    Ok(false) => PathStatus::NewLibrary,
                    Err(error) => PathStatus::Unavailable(error),
                };
            }
            Message::PrivacyAcknowledged(value) => self.privacy_acknowledged = value,
            Message::OpenLibrary => {
                if self.is_busy() || self.path_status != PathStatus::ExistingLibrary {
                    return Task::none();
                }
                self.notice = None;
                let key = self.begin_request(PathBuf::from(self.library_path.trim()));
                return load_task(key, false);
            }
            Message::CreateLibrary => {
                if self.is_busy() || self.path_status != PathStatus::NewLibrary {
                    return Task::none();
                } else if !self.privacy_acknowledged {
                    self.notice = Some(Notice::error(
                        "Please acknowledge how local data and public editor metadata are stored.",
                    ));
                } else {
                    self.notice = None;
                    let key = self.begin_request(PathBuf::from(self.library_path.trim()));
                    return load_task(key, true);
                }
            }
            Message::Loaded(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.library_path = snapshot.path.display().to_string();
                        self.snapshot = Some(snapshot);
                        self.screen = Screen::Dashboard;
                        self.path_status = PathStatus::ExistingLibrary;
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::SelectTab(tab) => self.tab = tab,
            Message::Refresh => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return load_task(key, false);
            }
            Message::ChooseAnotherLibrary => {
                if self.is_busy() {
                    return Task::none();
                }
                self.screen = Screen::Setup;
                self.snapshot = None;
                self.notice = None;
                self.verification = VerificationState::NotRun;
                self.reader = None;
            }
            Message::CollectionNameChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.name = value;
                self.selection_preview = None;
            }
            Message::LanguageChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.language_code = value;
                self.selection_preview = None;
            }
            Message::EndpointChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.api_endpoint = value;
                self.selection_preview = None;
            }
            Message::SelectionModeChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.selection_mode = value;
                self.selection_preview = None;
            }
            Message::SelectionChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.selection = value;
                self.selection_preview = None;
            }
            Message::CategoryDepthChanged(value) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.collection_form.category_depth = value;
                self.selection_preview = None;
            }
            Message::HistoryModeChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.history_mode = value;
                }
            }
            Message::HistoryValueChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.history_value = value;
                }
            }
            Message::MaximumPagesChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.maximum_pages = value;
                }
            }
            Message::MaximumBytesChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.maximum_bytes = value;
                }
            }
            Message::PreviewCollection => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let request = PreviewCollectionRequest {
                    api_endpoint: self.collection_form.api_endpoint.trim().to_owned(),
                    rule: match self.collection_form.rule() {
                        Ok(rule) => rule,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                };
                let key = self.begin_request(path);
                return preview_task(key, request);
            }
            Message::CollectionPreviewed(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(preview) => {
                        self.notice = Some(Notice::success(format!(
                            "Preview complete: {} resolved page(s), {} missing title(s). Review the estimate and policies before creating.",
                            preview.members.len(),
                            preview.missing_titles.len()
                        )));
                        self.selection_preview = Some(preview);
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::CreateCollection => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let Some(preview) = self.selection_preview.clone() else {
                    self.notice = Some(Notice::error("Preview the collection before creating it."));
                    return Task::none();
                };
                let request = CreateCollectionRequest {
                    library_path: path.clone(),
                    name: self.collection_form.name.trim().to_owned(),
                    language_code: self.collection_form.language_code.trim().to_owned(),
                    api_endpoint: self.collection_form.api_endpoint.trim().to_owned(),
                    preview,
                    history_policy: match self.collection_form.history_policy() {
                        Ok(policy) => policy,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                    budget: match self.collection_form.budget() {
                        Ok(budget) => budget,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                };
                let key = self.begin_request(path);
                return collection_task(key, request);
            }
            Message::CollectionCreated(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.collection_form.name.clear();
                        self.collection_form.selection.clear();
                        self.selection_preview = None;
                        self.notice = Some(Notice::success(
                            "Collection created and synchronized. It is ready in the offline reader.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::UpdateCollection(collection_id) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return update_collection_task(key, path, collection_id);
            }
            Message::CollectionUpdated(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.notice = Some(Notice::success(
                            "Collection update completed; every discovered intermediate revision is durable.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::VerifyFull => {
                if self.is_busy() {
                    return Task::none();
                }
                self.verification = VerificationState::Running;
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return verification_task(key);
            }
            Message::VerificationFinished(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                self.verification = match completion.result {
                    Ok(report) => VerificationState::Complete(report),
                    Err(error) => VerificationState::Failed(error),
                };
            }
            Message::OpenReader => {
                if let Some(reader) = &self.reader {
                    self.notice = open_system_browser(reader.local_url())
                        .err()
                        .map(Notice::error)
                        .or_else(|| Some(Notice::success("Opened the local offline reader.")));
                    return Task::none();
                }
                if self.is_busy() {
                    return Task::none();
                }
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return reader_task(key);
            }
            Message::ReaderStarted(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(reader) => {
                        let url = reader.local_url().to_owned();
                        self.reader = Some(reader);
                        self.notice = match open_system_browser(&url) {
                            Ok(()) => Some(Notice::success(format!(
                                "Offline reader is running at {url}"
                            ))),
                            Err(error) => Some(Notice::error(format!(
                                "Reader is running at {url}, but the browser could not be opened: {error}"
                            ))),
                        };
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
        }
        Task::none()
    }

    fn is_busy(&self) -> bool {
        self.active_request.is_some()
    }

    fn next_key(&mut self, path: PathBuf) -> RequestKey {
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("GUI request identifier exhausted");
        RequestKey { id, path }
    }

    fn begin_request(&mut self, path: PathBuf) -> RequestKey {
        let key = self.next_key(path);
        self.active_request = Some(key.clone());
        key
    }

    fn finish_request(&mut self, key: &RequestKey) -> bool {
        if self.active_request.as_ref() != Some(key)
            || key.path.as_path() != Path::new(self.library_path.trim())
        {
            return false;
        }
        self.active_request = None;
        true
    }

    fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Setup => self.setup_view(),
            Screen::Dashboard => self.dashboard_view(),
        }
    }

    fn setup_view(&self) -> Element<'_, Message> {
        let primary = if self.path_status == PathStatus::ExistingLibrary {
            button("Open library").on_press_maybe((!self.is_busy()).then_some(Message::OpenLibrary))
        } else if self.path_status == PathStatus::NewLibrary {
            button("Create local library").on_press_maybe(
                (!self.is_busy() && self.privacy_acknowledged).then_some(Message::CreateLibrary),
            )
        } else {
            button("Checking location…")
        };

        let path_message = match &self.path_status {
            PathStatus::Checking => "Checking this location…".to_owned(),
            PathStatus::ExistingLibrary => {
                "An existing WikiSyncer library was found at this location.".to_owned()
            }
            PathStatus::NewLibrary => {
                "A new library will be initialized at this location.".to_owned()
            }
            PathStatus::Unavailable(error) => format!("This location is unavailable: {error}"),
        };

        let content = column![
            text("WikiSyncer").size(38),
            text("Your selective, offline Wikipedia library").size(20),
            Space::new(Length::Shrink, 18),
            text("Choose where WikiSyncer should keep its database and content objects."),
            text_input("/path/to/library", &self.library_path)
                .on_input(Message::LibraryPathChanged)
                .padding(12),
            text(path_message),
            Space::new(Length::Shrink, 12),
            container(
                column![
                    text("Privacy and storage").size(20),
                    text("Articles, revision history, edit comments, and public editor names or IP addresses can be stored locally. Content that is later deleted or suppressed upstream may remain in a local library; retaining or sharing it can carry privacy or legal responsibilities. WikiSyncer does not hide or encrypt the chosen directory. Synchronization may use significant disk space and network bandwidth."),
                    checkbox(
                        "I understand this library may contain public editor metadata and is readable by local users with access to the directory.",
                        self.privacy_acknowledged,
                    )
                    .on_toggle(Message::PrivacyAcknowledged),
                ]
                .spacing(10),
            )
            .padding(16),
            row![primary, text(if self.is_busy() { "Opening…" } else { "" })]
                .spacing(12)
                .align_y(Alignment::Center),
            notice_view(self.notice.as_ref()),
        ]
        .spacing(14)
        .max_width(720);

        container(content)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .padding(28)
            .into()
    }

    fn dashboard_view(&self) -> Element<'_, Message> {
        let nav = row![
            nav_button("Overview", Tab::Overview, self.tab),
            nav_button("Collections", Tab::Collections, self.tab),
            nav_button("Sync", Tab::Sync, self.tab),
            nav_button("Integrity", Tab::Integrity, self.tab),
            Space::new(Length::Fill, Length::Shrink),
            button(if self.reader.is_some() {
                "Open reader"
            } else {
                "Start reader"
            })
            .on_press_maybe((!self.is_busy()).then_some(Message::OpenReader)),
            button("Refresh").on_press_maybe((!self.is_busy()).then_some(Message::Refresh)),
            button("Change library")
                .on_press_maybe((!self.is_busy()).then_some(Message::ChooseAnotherLibrary)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let body: Element<'_, Message> = match (&self.snapshot, self.tab) {
            (Some(snapshot), Tab::Overview) => self.overview_view(snapshot),
            (Some(snapshot), Tab::Collections) => self.collections_view(snapshot),
            (Some(snapshot), Tab::Sync) => self.sync_view(snapshot),
            (Some(snapshot), Tab::Integrity) => self.integrity_view(snapshot),
            (None, _) => text("Loading library…").into(),
        };

        column![
            row![
                column![
                    text("WikiSyncer").size(28),
                    text(&self.library_path).size(13),
                ],
                Space::new(Length::Fill, Length::Shrink),
                text(if self.is_busy() {
                    "Working…"
                } else {
                    "Offline library"
                }),
            ]
            .align_y(Alignment::Center),
            horizontal_rule(1),
            nav,
            notice_view(self.notice.as_ref()),
            scrollable(body).height(Length::Fill),
        ]
        .spacing(14)
        .padding(22)
        .into()
    }

    fn overview_view<'a>(&self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let metrics = row![
            metric("Collections", snapshot.collections.len().to_string()),
            metric(
                "Unique captured pages",
                snapshot.unique_page_count.to_string()
            ),
            metric(
                "Recent revisions shown",
                snapshot.recent_revisions.len().to_string()
            ),
            metric("Library size", storage_bytes_label(&snapshot.storage_usage)),
        ]
        .spacing(12);

        let mut recent = column![text("Recent captured revisions").size(23)].spacing(9);
        if snapshot.recent_revisions.is_empty() {
            recent = recent.push(text("No revisions have been captured yet."));
        } else {
            for revision in &snapshot.recent_revisions {
                recent = recent.push(
                    container(row![
                        column![
                            text(&revision.title).size(17),
                            text(format!(
                                "Wiki {} · revision {} · {}",
                                revision.wiki_id, revision.revision_id, revision.timestamp
                            ))
                            .size(13),
                        ],
                        Space::new(Length::Fill, Length::Shrink),
                        text(format_bytes(revision.source_size)),
                    ])
                    .padding(10),
                );
            }
        }

        let storage_warning: Element<'_, Message> =
            snapshot.storage_usage.as_ref().err().map_or_else(
                || Space::new(Length::Shrink, 0).into(),
                |error| text(format!("Storage scan unavailable: {error}")).into(),
            );

        column![
            text("Library overview").size(30),
            metrics,
            storage_warning,
            horizontal_rule(1),
            recent,
        ]
        .spacing(16)
        .into()
    }

    fn collections_view<'a>(&self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let mut list = column![text("Collections").size(30)].spacing(10);
        if snapshot.collections.is_empty() {
            list = list.push(text("No collections yet. Create one below."));
        } else {
            for collection in &snapshot.collections {
                list = list.push(collection_row(collection, !self.is_busy()));
            }
        }

        let preview_enabled = !self.is_busy()
            && !self.collection_form.name.trim().is_empty()
            && !self.collection_form.language_code.trim().is_empty()
            && !self.collection_form.api_endpoint.trim().is_empty()
            && !self.collection_form.selection.trim().is_empty();
        let create_enabled = preview_enabled && self.selection_preview.is_some();
        let mode_buttons = row![
            mode_button(
                "Page titles",
                SelectionMode::Titles,
                self.collection_form.selection_mode
            ),
            mode_button(
                "Title-list import",
                SelectionMode::TitleList,
                self.collection_form.selection_mode
            ),
            mode_button(
                "Category",
                SelectionMode::Category,
                self.collection_form.selection_mode
            ),
        ]
        .spacing(8);
        let selection_hint = match self.collection_form.selection_mode {
            SelectionMode::Titles => "One page title per line",
            SelectionMode::TitleList => "Paste a newline-delimited title list",
            SelectionMode::Category => "Category:Name",
        };
        let history_buttons = row![
            history_button(
                "Current + future",
                HistoryMode::CurrentAndFuture,
                self.collection_form.history_mode
            ),
            history_button(
                "Last N",
                HistoryMode::LastN,
                self.collection_form.history_mode
            ),
            history_button(
                "Since",
                HistoryMode::Since,
                self.collection_form.history_mode
            ),
            history_button(
                "Complete",
                HistoryMode::Complete,
                self.collection_form.history_mode
            ),
        ]
        .spacing(8);
        let preview_summary: Element<'_, Message> = self.selection_preview.as_ref().map_or_else(
            || text("Preview is required before any collection is created or downloaded.").into(),
            |preview| {
                let bytes = preview.predicted_canonical_bytes.map_or_else(
                    || "source bytes unknown until capture".to_owned(),
                    format_bytes,
                );
                text(format!(
                    "Ready to commit: {} pages, {} missing titles, {bytes}. Hard budgets are enforced before checkpoint advancement.",
                    preview.members.len(), preview.missing_titles.len()
                )).into()
            },
        );
        let form = column![
            text("Create and synchronize a collection").size(23),
            text("Preview scope and expected size first. Nothing is committed or downloaded until you choose Create and sync."),
            text_input("Collection name", &self.collection_form.name)
                .on_input(Message::CollectionNameChanged)
                .padding(10),
            row![
                text_input(
                    "Language code (for example: en)",
                    &self.collection_form.language_code
                )
                .on_input(Message::LanguageChanged)
                .padding(10),
                text_input(
                    "MediaWiki API endpoint",
                    &self.collection_form.api_endpoint
                )
                .on_input(Message::EndpointChanged)
                .padding(10),
            ]
            .spacing(10),
            mode_buttons,
            text_input(selection_hint, &self.collection_form.selection)
                .on_input(Message::SelectionChanged)
                .padding(10),
            text_input("Category recursion depth (0–16)", &self.collection_form.category_depth)
                .on_input(Message::CategoryDepthChanged)
                .padding(10),
            text("History retention").size(17),
            history_buttons,
            text_input("Last-N count or Since Unix timestamp", &self.collection_form.history_value)
                .on_input(Message::HistoryValueChanged)
                .padding(10),
            row![
                text_input("Hard maximum pages (blank = unlimited)", &self.collection_form.maximum_pages)
                    .on_input(Message::MaximumPagesChanged)
                    .padding(10),
                text_input("Hard maximum canonical bytes (blank = unlimited)", &self.collection_form.maximum_bytes)
                    .on_input(Message::MaximumBytesChanged)
                    .padding(10),
            ].spacing(10),
            text("If a page leaves a category, WikiSyncer stops tracking it but retains every already captured revision."),
            row![
                button("Preview selection")
                    .on_press_maybe(preview_enabled.then_some(Message::PreviewCollection)),
                button("Create and sync")
                .on_press_maybe(create_enabled.then_some(Message::CreateCollection)),
            ].spacing(10),
            preview_summary,
        ]
        .spacing(10);

        column![list, horizontal_rule(1), form].spacing(18).into()
    }

    fn sync_view<'a>(&self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let mut content = column![
            text("Synchronization").size(30),
            text("Create-and-sync and Update actions use the shared durable capture/reconciliation services. Interrupted page jobs resume without advancing their checkpoint prematurely."),
        ]
        .spacing(10);

        if snapshot.runs.is_empty() {
            content = content.push(text("No synchronization runs recorded."));
        } else {
            for run in &snapshot.runs {
                let done = run.succeeded_jobs;
                let total =
                    run.queued_jobs + run.running_jobs + run.succeeded_jobs + run.failed_jobs;
                let progress = if total == 0 {
                    0.0
                } else {
                    done as f32 / total as f32
                };
                let status = if run.state == SyncRunState::Running && run.failed_jobs > 0 {
                    "attention"
                } else {
                    run.state.as_str()
                };
                let mut card = column![
                    row![
                        text(format!(
                            "{} run #{}",
                            title_case(run.kind.as_str()),
                            run.run_id
                        ))
                        .size(19),
                        Space::new(Length::Fill, Length::Shrink),
                        text(status),
                    ],
                    progress_bar(0.0..=1.0, progress),
                    text(format!(
                        "{} queued · {} running · {} succeeded · {} failed",
                        run.queued_jobs, run.running_jobs, run.succeeded_jobs, run.failed_jobs
                    )),
                ]
                .spacing(7);
                if let Some(error) = &run.latest_error {
                    card = card.push(text(format!(
                        "Last error [{}]: {}",
                        error.code, error.message
                    )));
                }
                content = content.push(container(card).padding(12));
            }
        }

        content = content
            .push(horizontal_rule(1))
            .push(text("Checkpoints").size(23));
        if snapshot.checkpoints.is_empty() {
            content = content.push(text("No source checkpoints recorded."));
        } else {
            for checkpoint in &snapshot.checkpoints {
                content = content.push(text(checkpoint_summary(checkpoint)));
            }
        }
        content.into()
    }

    fn integrity_view<'a>(&self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let result: Element<'_, Message> = match &self.verification {
            VerificationState::NotRun => {
                text("No read verification has run in this session.").into()
            }
            VerificationState::Running => {
                text("Reading and hash-verifying every logical content object…").into()
            }
            VerificationState::Complete(report) => column![
                text(format!(
                    "Verified {} of {} logical objects ({} canonical bytes).",
                    report.objects_verified, report.objects_at_start, report.canonical_bytes_verified
                )),
                text(if report.is_verified_since_capture() {
                    "Complete: every captured canonical object in the stable catalog was verified since capture. This does not establish that its statements are true."
                } else {
                    "Verification did not establish complete clean coverage. Review the finding count and local diagnostics."
                }),
                text(format!("{} finding(s); {} detailed finding(s) retained.", report.finding_count, report.findings.len())),
            ]
            .spacing(6)
            .into(),
            VerificationState::Failed(error) => {
                text(format!("Verification stopped: {error}")).into()
            }
        };

        column![
            text("Integrity").size(30),
            text("WikiSyncer content objects have content-derived identities. Shared store reads decompress, bound, and hash-check canonical bytes before returning them."),
            row![
                metric("Schema version", snapshot.schema_version.to_string()),
                metric(
                    "Unique objects in overview sample",
                    snapshot.recent_unique_object_count.to_string()
                ),
                metric("Files on disk", storage_files_label(&snapshot.storage_usage)),
            ]
            .spacing(12),
            button("Verify full library").on_press_maybe(
                (!self.is_busy()
                    && !matches!(self.verification, VerificationState::Running))
                    .then_some(Message::VerifyFull),
            ),
            result,
            text("Full verification covers logical canonical objects, including packed reconstruction and hashes. Manifest-chain and search-pointer verification remain separately tracked hardening work.").size(13),
        ]
        .spacing(14)
        .into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Setup,
    Dashboard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Overview,
    Collections,
    Sync,
    Integrity,
}

#[derive(Clone, Debug)]
enum Message {
    LibraryPathChanged(String),
    PathProbed(ScopedResult<bool>),
    PrivacyAcknowledged(bool),
    OpenLibrary,
    CreateLibrary,
    Loaded(ScopedResult<DashboardSnapshot>),
    SelectTab(Tab),
    Refresh,
    ChooseAnotherLibrary,
    CollectionNameChanged(String),
    LanguageChanged(String),
    EndpointChanged(String),
    SelectionModeChanged(SelectionMode),
    SelectionChanged(String),
    CategoryDepthChanged(String),
    HistoryModeChanged(HistoryMode),
    HistoryValueChanged(String),
    MaximumPagesChanged(String),
    MaximumBytesChanged(String),
    PreviewCollection,
    CollectionPreviewed(ScopedResult<CollectionSelectionPreview>),
    CreateCollection,
    CollectionCreated(ScopedResult<DashboardSnapshot>),
    UpdateCollection(CollectionId),
    CollectionUpdated(ScopedResult<DashboardSnapshot>),
    VerifyFull,
    VerificationFinished(ScopedResult<VerificationReport>),
    OpenReader,
    ReaderStarted(ScopedResult<Arc<ReaderHandle>>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RequestKey {
    id: u64,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct ScopedResult<T> {
    key: RequestKey,
    result: Result<T, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PathStatus {
    Checking,
    ExistingLibrary,
    NewLibrary,
    Unavailable(String),
}

#[derive(Clone, Debug)]
struct DashboardSnapshot {
    path: PathBuf,
    collections: Vec<StoredCollection>,
    runs: Vec<SyncRunStatus>,
    checkpoints: Vec<SyncCheckpoint>,
    recent_revisions: Vec<RecentRevision>,
    unique_page_count: usize,
    recent_unique_object_count: usize,
    storage_usage: Result<StorageUsage, String>,
    schema_version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StorageUsage {
    bytes: u64,
    files: u64,
}

#[derive(Clone, Debug)]
struct RecentRevision {
    wiki_id: u64,
    revision_id: u64,
    title: String,
    timestamp: String,
    source_size: u64,
}

#[derive(Clone, Debug)]
struct CollectionForm {
    name: String,
    language_code: String,
    api_endpoint: String,
    selection_mode: SelectionMode,
    selection: String,
    category_depth: String,
    history_mode: HistoryMode,
    history_value: String,
    maximum_pages: String,
    maximum_bytes: String,
}

impl Default for CollectionForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            language_code: "en".to_owned(),
            api_endpoint: "https://en.wikipedia.org/w/api.php".to_owned(),
            selection_mode: SelectionMode::Titles,
            selection: String::new(),
            category_depth: "0".to_owned(),
            history_mode: HistoryMode::CurrentAndFuture,
            history_value: String::new(),
            maximum_pages: "10000".to_owned(),
            maximum_bytes: String::new(),
        }
    }
}

impl CollectionForm {
    fn rule(&self) -> Result<CollectionRule, String> {
        match self.selection_mode {
            SelectionMode::Titles => parse_title_list(&self.selection, 10_000)
                .map(CollectionRule::ExplicitTitles)
                .map_err(|error| error.to_string()),
            SelectionMode::TitleList => parse_title_list(&self.selection, 10_000)
                .map(CollectionRule::TitleList)
                .map_err(|error| error.to_string()),
            SelectionMode::Category => {
                let title = PageTitle::new(&self.selection).map_err(|error| error.to_string())?;
                let recursion_depth =
                    self.category_depth.trim().parse::<u16>().map_err(|_| {
                        "Category depth must be an integer from 0 to 16.".to_owned()
                    })?;
                if recursion_depth > CategoryPreviewLimits::default().max_recursion_depth {
                    return Err("Category depth must be an integer from 0 to 16.".to_owned());
                }
                Ok(CollectionRule::Category {
                    title,
                    recursion_depth,
                })
            }
        }
    }

    fn history_policy(&self) -> Result<HistoryPolicy, String> {
        match self.history_mode {
            HistoryMode::CurrentAndFuture => Ok(HistoryPolicy::CurrentAndFuture),
            HistoryMode::Complete => Ok(HistoryPolicy::Complete),
            HistoryMode::LastN => {
                let count =
                    self.history_value.trim().parse::<u32>().map_err(|_| {
                        "Last-N history requires a positive revision count.".to_owned()
                    })?;
                HistoryPolicy::last_n(count).map_err(|error| error.to_string())
            }
            HistoryMode::Since => {
                let timestamp = self
                    .history_value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "Since history requires Unix seconds in UTC.".to_owned())?;
                Ok(HistoryPolicy::Since(UnixTimestamp::from_seconds(timestamp)))
            }
        }
    }

    fn budget(&self) -> Result<CollectionBudget, String> {
        let mut budget = CollectionBudget::unlimited();
        if !self.maximum_pages.trim().is_empty() {
            let pages = self
                .maximum_pages
                .trim()
                .parse::<u64>()
                .map_err(|_| "Maximum pages must be a positive integer.".to_owned())?;
            budget = budget
                .with_maximum_pages(pages)
                .map_err(|error| error.to_string())?;
        }
        if !self.maximum_bytes.trim().is_empty() {
            let bytes = self
                .maximum_bytes
                .trim()
                .parse::<u64>()
                .map_err(|_| "Maximum bytes must be a positive integer.".to_owned())?;
            budget = budget
                .with_maximum_bytes(bytes)
                .map_err(|error| error.to_string())?;
        }
        Ok(budget)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionMode {
    Titles,
    TitleList,
    Category,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryMode {
    CurrentAndFuture,
    LastN,
    Since,
    Complete,
}

#[derive(Clone, Debug)]
struct PreviewCollectionRequest {
    api_endpoint: String,
    rule: CollectionRule,
}

#[derive(Clone, Debug)]
struct CreateCollectionRequest {
    library_path: PathBuf,
    name: String,
    language_code: String,
    api_endpoint: String,
    preview: CollectionSelectionPreview,
    history_policy: HistoryPolicy,
    budget: CollectionBudget,
}

#[derive(Clone, Debug)]
enum VerificationState {
    NotRun,
    Running,
    Complete(VerificationReport),
    Failed(String),
}

#[derive(Debug)]
struct Notice {
    message: String,
    kind: NoticeKind,
}

impl Notice {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: NoticeKind::Error,
        }
    }

    fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: NoticeKind::Success,
        }
    }
}

#[derive(Debug)]
enum NoticeKind {
    Error,
    Success,
}

fn probe_task(key: RequestKey) -> Task<Message> {
    Task::perform(
        async move {
            let result = probe_library(&key.path);
            ScopedResult { key, result }
        },
        Message::PathProbed,
    )
}

fn load_task(key: RequestKey, create: bool) -> Task<Message> {
    Task::perform(
        async move {
            let result = load_library(key.path.clone(), create).await;
            ScopedResult { key, result }
        },
        Message::Loaded,
    )
}

fn collection_task(key: RequestKey, request: CreateCollectionRequest) -> Task<Message> {
    Task::perform(
        async move {
            let result = create_collection(request).await;
            ScopedResult { key, result }
        },
        Message::CollectionCreated,
    )
}

fn preview_task(key: RequestKey, request: PreviewCollectionRequest) -> Task<Message> {
    Task::perform(
        async move {
            let result = preview_collection(request).await;
            ScopedResult { key, result }
        },
        Message::CollectionPreviewed,
    )
}

fn update_collection_task(
    key: RequestKey,
    path: PathBuf,
    collection_id: CollectionId,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = update_collection(path, collection_id).await;
            ScopedResult { key, result }
        },
        Message::CollectionUpdated,
    )
}

fn verification_task(key: RequestKey) -> Task<Message> {
    Task::perform(
        async move {
            let result = verify_all_objects(key.path.clone()).await;
            ScopedResult { key, result }
        },
        Message::VerificationFinished,
    )
}

fn reader_task(key: RequestKey) -> Task<Message> {
    Task::perform(
        async move {
            let result = wikisync_web::start_loopback(key.path.clone())
                .await
                .map(Arc::new)
                .map_err(|error| error.to_string());
            ScopedResult { key, result }
        },
        Message::ReaderStarted,
    )
}

fn probe_library(path: &Path) -> Result<bool, String> {
    if path.as_os_str().is_empty() {
        return Ok(false);
    }
    match fs::metadata(path.join(DATABASE_NAME)) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "{} exists but is not a regular file",
            path.join(DATABASE_NAME).display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

async fn load_library(path: PathBuf, create: bool) -> Result<DashboardSnapshot, String> {
    load_library_snapshot(&path, create)
}

fn load_library_snapshot(path: &Path, create: bool) -> Result<DashboardSnapshot, String> {
    if path.as_os_str().is_empty() {
        return Err("Choose a library directory.".to_owned());
    }
    if !create && !path.join(DATABASE_NAME).is_file() {
        return Err(format!(
            "{} is not an initialized WikiSyncer library",
            path.display()
        ));
    }
    let library = Library::open(path).map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn create_collection(request: CreateCollectionRequest) -> Result<DashboardSnapshot, String> {
    create_collection_and_sync(&request).await
}

async fn preview_collection(
    request: PreviewCollectionRequest,
) -> Result<CollectionSelectionPreview, String> {
    let config = ClientConfig::new(
        &request.api_endpoint,
        concat!("WikiSyncer/", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|error| error.to_string())?;
    let client =
        wikisync_mediawiki::MediaWikiClient::new(config).map_err(|error| error.to_string())?;
    preview_collection_rule(&client, &request.rule, CategoryPreviewLimits::default())
        .await
        .map_err(|error| error.to_string())
}

async fn create_collection_and_sync(
    request: &CreateCollectionRequest,
) -> Result<DashboardSnapshot, String> {
    if request.name.is_empty()
        || request.language_code.is_empty()
        || request.api_endpoint.is_empty()
    {
        return Err("Collection name, language code, and API endpoint are required.".to_owned());
    }
    let client_config = ClientConfig::new(
        &request.api_endpoint,
        concat!("WikiSyncer/", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|error| error.to_string())?;
    let mut library = Library::open(&request.library_path).map_err(|error| error.to_string())?;
    let wiki_id = library
        .register_wiki(client_config.endpoint().as_str(), &request.language_code)
        .map_err(|error| error.to_string())?;
    let collection_id = library
        .create_collection(
            wiki_id,
            &request.name,
            &request.preview.rule,
            request.history_policy,
            request.budget,
            CollectionRemovalPolicy::StopTrackingRetainHistory,
        )
        .map_err(|error| error.to_string())?;
    commit_collection_preview(
        &mut library,
        collection_id,
        &request.preview,
        request.history_policy,
        request.budget,
        CollectionRemovalPolicy::StopTrackingRetainHistory,
    )
    .map_err(|error| error.to_string())?;
    let client = wikisync_mediawiki::MediaWikiClient::new(client_config)
        .map_err(|error| error.to_string())?;
    bootstrap_collection(&client, &mut library, collection_id)
        .await
        .map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn update_collection(
    path: PathBuf,
    collection_id: CollectionId,
) -> Result<DashboardSnapshot, String> {
    let mut library = Library::open(&path).map_err(|error| error.to_string())?;
    let configuration = library
        .collection_configuration(collection_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Collection has no committed configuration.".to_owned())?;
    let wiki = library
        .wiki(configuration.wiki_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Collection source is missing.".to_owned())?;
    let client_config = ClientConfig::new(
        &wiki.api_endpoint,
        concat!("WikiSyncer/", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|error| error.to_string())?;
    let client = wikisync_mediawiki::MediaWikiClient::new(client_config)
        .map_err(|error| error.to_string())?;
    let checkpoint = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is before the Unix epoch.".to_owned())?
        .as_secs();
    reconcile_collection_heads(
        &client,
        &mut library,
        configuration.wiki_id,
        collection_id,
        checkpoint,
    )
    .await
    .map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn verify_all_objects(path: PathBuf) -> Result<VerificationReport, String> {
    let library = Library::open(path).map_err(|error| error.to_string())?;
    verify_library(&library, VerificationScope::Full).map_err(|error| error.to_string())
}

fn open_system_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err("Opening a browser is supported on macOS and Linux.".to_owned());

    command
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn snapshot(library: &Library) -> Result<DashboardSnapshot, String> {
    let collections = library.collections().map_err(|error| error.to_string())?;
    let mut unique_pages = BTreeSet::new();
    for collection in &collections {
        for page in library
            .collection_pages(collection.wiki_id, collection.collection_id)
            .map_err(|error| error.to_string())?
        {
            unique_pages.insert((page.wiki_id, page.page_id));
        }
    }
    let runs = library
        .sync_run_statuses(20)
        .map_err(|error| error.to_string())?;
    let checkpoints = library
        .sync_checkpoints()
        .map_err(|error| error.to_string())?;
    let revisions = library
        .recent_revisions(RECENT_REVISION_LIMIT)
        .map_err(|error| error.to_string())?;
    let recent_unique_object_count = revisions
        .iter()
        .map(|(_, revision)| revision.content_object_id)
        .collect::<BTreeSet<_>>()
        .len();
    let recent_revisions = revisions
        .into_iter()
        .map(|(wiki_id, revision)| {
            let title = library
                .page(wiki_id, revision.page_id)
                .map_err(|error| error.to_string())?
                .map_or_else(
                    || format!("Page {}", revision.page_id),
                    |page| page.title.into_string(),
                );
            Ok(RecentRevision {
                wiki_id: wiki_id.get(),
                revision_id: revision.revision_id.get(),
                title,
                timestamp: revision.timestamp,
                source_size: revision.source_size,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let storage_usage = directory_usage(library.root())
        .map(|(bytes, files)| StorageUsage { bytes, files })
        .map_err(|error| error.to_string());
    Ok(DashboardSnapshot {
        path: library.root().to_path_buf(),
        collections,
        runs,
        checkpoints,
        recent_revisions,
        unique_page_count: unique_pages.len(),
        recent_unique_object_count,
        storage_usage,
        schema_version: library
            .schema_version()
            .map_err(|error| error.to_string())?,
    })
}

fn directory_usage(root: &Path) -> std::io::Result<(u64, u64)> {
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let (child_bytes, child_files) = directory_usage(&entry.path())?;
            bytes = bytes.saturating_add(child_bytes);
            files = files.saturating_add(child_files);
        } else if file_type.is_file() {
            bytes = bytes.saturating_add(entry.metadata()?.len());
            files = files.saturating_add(1);
        }
    }
    Ok((bytes, files))
}

fn suggested_library_path() -> String {
    if let Some(path) = env::var_os("WIKISYNC_LIBRARY") {
        return PathBuf::from(path).display().to_string();
    }
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("WikiSyncer Library")
        .display()
        .to_string()
}

fn nav_button(label: &str, tab: Tab, selected: Tab) -> Element<'static, Message> {
    let label = if tab == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(Message::SelectTab(tab)).into()
}

fn mode_button(
    label: &str,
    mode: SelectionMode,
    selected: SelectionMode,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label))
        .on_press(Message::SelectionModeChanged(mode))
        .into()
}

fn history_button(
    label: &str,
    mode: HistoryMode,
    selected: HistoryMode,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label))
        .on_press(Message::HistoryModeChanged(mode))
        .into()
}

fn metric<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    container(column![text(value).size(25), text(label).size(13)].spacing(3))
        .padding(12)
        .width(Length::Fill)
        .into()
}

fn collection_row(collection: &StoredCollection, update_enabled: bool) -> Element<'_, Message> {
    container(row![
        column![
            text(&collection.name).size(19),
            text(format!(
                "Collection {} · wiki {}",
                collection.collection_id, collection.wiki_id
            ))
            .size(13),
        ],
        Space::new(Length::Fill, Length::Shrink),
        text(format!("{} pages", collection.page_count)),
        button("Update").on_press_maybe(
            update_enabled.then_some(Message::UpdateCollection(collection.collection_id))
        ),
    ])
    .padding(12)
    .into()
}

fn notice_view(notice: Option<&Notice>) -> Element<'_, Message> {
    match notice {
        Some(notice) => {
            let prefix = match notice.kind {
                NoticeKind::Error => "Problem: ",
                NoticeKind::Success => "Done: ",
            };
            container(text(format!("{prefix}{}", notice.message)))
                .padding(10)
                .into()
        }
        None => Space::new(Length::Shrink, 0).into(),
    }
}

fn checkpoint_summary(checkpoint: &SyncCheckpoint) -> String {
    let scope = checkpoint.collection_id.map_or_else(
        || "all collections".to_owned(),
        |id| format!("collection {id}"),
    );
    format!(
        "Wiki {} · {} · committed through {} · next window {} ({}s overlap)",
        checkpoint.wiki_id,
        scope,
        checkpoint.committed_through,
        checkpoint.next_window_start(),
        checkpoint.overlap_seconds
    )
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn storage_bytes_label(storage: &Result<StorageUsage, String>) -> String {
    storage.as_ref().map_or_else(
        |_| "Unavailable".to_owned(),
        |usage| format_bytes(usage.bytes),
    )
}

fn storage_files_label(storage: &Result<StorageUsage, String>) -> String {
    storage.as_ref().map_or_else(
        |_| "Unavailable".to_owned(),
        |usage| usage.files.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc as Shared, Mutex};
    use std::thread;
    use wikisync_core::{PageId, PageTitle, RevisionId};
    use wikisync_store::CurrentRevisionCapture;

    const TITLE_RESOLUTION: &str =
        include_str!("../../../fixtures/mediawiki/title-resolution.json");
    const REVISION_CONTENT: &str =
        include_str!("../../../fixtures/mediawiki/revision-content.json");
    const UNCHANGED_HEAD: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-unchanged-title-resolution.json");
    const CHANGED_HEAD: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-title-resolution.json");
    const FORWARD_REVISIONS: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-revisions.json");
    const MIDDLE_CONTENT: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-content-middle.json");
    const HEAD_CONTENT: &str =
        include_str!("../../../fixtures/mediawiki/reconciliation-content-head.json");

    #[derive(Debug)]
    struct FixtureServer {
        endpoint: String,
        requests: Shared<Mutex<Vec<String>>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FixtureServer {
        fn start(responses: Vec<&'static str>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
            let endpoint = format!(
                "http://{}/w/api.php",
                listener.local_addr().expect("fixture address")
            );
            let requests = Shared::new(Mutex::new(Vec::new()));
            let captured = Shared::clone(&requests);
            let thread = thread::spawn(move || {
                for body in responses {
                    let (mut stream, _) = listener.accept().expect("fixture accept");
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let read = stream.read(&mut buffer).expect("fixture read");
                        assert!(read > 0);
                        request.extend_from_slice(&buffer[..read]);
                    }
                    captured
                        .lock()
                        .expect("request lock")
                        .push(String::from_utf8(request).expect("request text"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .expect("fixture response");
                }
            });
            Self {
                endpoint,
                requests,
                thread: Some(thread),
            }
        }

        fn finish(mut self) -> Vec<String> {
            self.thread.take().unwrap().join().expect("fixture thread");
            Shared::try_unwrap(self.requests)
                .expect("request owners")
                .into_inner()
                .expect("request lock")
        }
    }

    #[test]
    fn formats_storage_sizes_for_people() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn directory_usage_counts_nested_regular_files() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(temporary.path().join("nested")).expect("nested directory");
        fs::write(temporary.path().join("one"), b"123").expect("first file");
        fs::write(temporary.path().join("nested/two"), b"4567").expect("second file");
        assert_eq!(directory_usage(temporary.path()).unwrap(), (7, 2));
    }

    #[test]
    fn setup_creates_and_reopens_a_library() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("library");

        let created = load_library_snapshot(&root, true).expect("create library");
        assert_eq!(created.path, root);
        assert_eq!(created.schema_version, 6);
        assert!(root.join(DATABASE_NAME).is_file());

        let reopened = load_library_snapshot(&root, false).expect("reopen library");
        assert!(reopened.collections.is_empty());
    }

    #[test]
    fn collection_form_parses_scope_history_and_hard_budgets() {
        let form = CollectionForm {
            selection_mode: SelectionMode::TitleList,
            selection: "Rust\nFerris\nRust\n".to_owned(),
            history_mode: HistoryMode::LastN,
            history_value: "25".to_owned(),
            maximum_pages: "100".to_owned(),
            maximum_bytes: "1048576".to_owned(),
            ..CollectionForm::default()
        };
        assert!(matches!(form.rule(), Ok(CollectionRule::TitleList(_))));
        assert!(matches!(form.history_policy(), Ok(HistoryPolicy::LastN(_))));
        let budget = form.budget().expect("budget");
        assert_eq!(budget.maximum_pages().unwrap().get(), 100);
        assert_eq!(budget.maximum_bytes().unwrap().get(), 1_048_576);
    }

    #[test]
    fn invalid_endpoint_is_rejected_before_collection_mutation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        load_library_snapshot(temporary.path(), true).expect("create library");

        let error = ClientConfig::new("http://example.com/w/api.php", "WikiSyncer/endpoint-test")
            .expect_err("remote plain HTTP endpoint must be rejected");

        assert!(error.to_string().contains("HTTPS"));
        let library = Library::open(temporary.path()).expect("reopen library");
        assert!(library.collections().unwrap().is_empty());
    }

    #[test]
    fn snapshot_counts_pages_once_across_collections() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let mut library = Library::open(temporary.path()).expect("create library");
        let wiki_id = library
            .register_wiki("https://example.invalid/w/api.php", "en")
            .expect("register source");
        let first = library
            .create_explicit_collection(wiki_id, "First")
            .expect("first collection");
        let second = library
            .create_explicit_collection(wiki_id, "Second")
            .expect("second collection");
        let title = PageTitle::new("Shared page").unwrap();
        let capture = CurrentRevisionCapture {
            page_id: PageId::new(10).unwrap(),
            namespace: 0,
            title: &title,
            revision_id: RevisionId::new(20).unwrap(),
            parent_id: None,
            timestamp: "2026-08-21T12:00:00Z",
            author: None,
            author_id: None,
            comment: None,
            minor: false,
            upstream_sha1: None,
            content_model: "wikitext",
            source: b"shared",
        };
        library
            .capture_current_revision(wiki_id, first, &capture)
            .expect("capture in first collection");
        library
            .capture_current_revision(wiki_id, second, &capture)
            .expect("attach to second collection");

        let snapshot = snapshot(&library).expect("snapshot");
        assert_eq!(
            snapshot
                .collections
                .iter()
                .map(|item| item.page_count)
                .sum::<u64>(),
            2
        );
        assert_eq!(snapshot.unique_page_count, 1);
    }

    #[test]
    fn stale_results_cannot_replace_the_active_library_request() {
        let (mut app, initial_task) = App::new();
        drop(initial_task);
        app.library_path = "/active/library".to_owned();
        let active = RequestKey {
            id: 42,
            path: PathBuf::from("/active/library"),
        };
        app.active_request = Some(active.clone());

        drop(app.update(Message::Loaded(ScopedResult {
            key: RequestKey {
                id: 41,
                path: active.path.clone(),
            },
            result: Err("stale failure".to_owned()),
        })));

        assert_eq!(app.active_request, Some(active));
        assert!(app.notice.is_none());
    }

    #[test]
    fn library_path_cannot_change_during_active_work() {
        let (mut app, initial_task) = App::new();
        drop(initial_task);
        app.library_path = "/active/library".to_owned();
        app.active_request = Some(RequestKey {
            id: 7,
            path: PathBuf::from("/active/library"),
        });

        drop(app.update(Message::LibraryPathChanged("/different/library".to_owned())));

        assert_eq!(app.library_path, "/active/library");
    }

    #[test]
    fn unavailable_storage_is_not_rendered_as_zero() {
        let unavailable = Err("permission denied".to_owned());
        assert_eq!(storage_bytes_label(&unavailable), "Unavailable");
        assert_eq!(storage_files_label(&unavailable), "Unavailable");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nontechnical_gate_flow_creates_syncs_updates_verifies_and_starts_reader() {
        let server = FixtureServer::start(vec![
            TITLE_RESOLUTION,
            UNCHANGED_HEAD,
            REVISION_CONTENT,
            CHANGED_HEAD,
            FORWARD_REVISIONS,
            MIDDLE_CONTENT,
            HEAD_CONTENT,
        ]);
        let temporary = tempfile::tempdir().expect("temporary library");
        load_library_snapshot(temporary.path(), true).expect("create library");
        let rule = CollectionRule::TitleList(
            parse_title_list("Rust_programming_language\n", 10_000).expect("title list"),
        );
        let preview = preview_collection(PreviewCollectionRequest {
            api_endpoint: server.endpoint.clone(),
            rule: rule.clone(),
        })
        .await
        .expect("preview");
        assert_eq!(preview.members.len(), 1);

        let created = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Systems languages".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: server.endpoint.clone(),
            preview,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited()
                .with_maximum_pages(10)
                .unwrap()
                .with_maximum_bytes(1_000_000)
                .unwrap(),
        })
        .await
        .expect("create and sync");
        assert_eq!(created.collections[0].page_count, 1);
        let collection_id = created.collections[0].collection_id;

        let updated = update_collection(temporary.path().to_path_buf(), collection_id)
            .await
            .expect("update collection");
        assert!(
            updated
                .runs
                .iter()
                .any(|run| run.state == SyncRunState::Succeeded)
        );
        let library = Library::open(temporary.path()).expect("library");
        let page = library
            .collection_pages(created.collections[0].wiki_id, collection_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            library
                .revisions_for_page(page.wiki_id, page.page_id)
                .unwrap()
                .len(),
            3
        );
        let verification = verify_library(&library, VerificationScope::Full).unwrap();
        assert!(verification.is_verified_since_capture());
        drop(library);

        let reader = wikisync_web::start_loopback(temporary.path())
            .await
            .expect("reader");
        assert!(reader.address().ip().is_loopback());
        reader.shutdown().await.expect("reader shutdown");
        assert_eq!(server.finish().len(), 7);
    }
}
