use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use iced::widget::{
    Space, button, checkbox, column, container, horizontal_rule, progress_bar, row, scrollable,
    text, text_input,
};
use iced::{Alignment, Element, Length, Task, Theme};
use wikisync_mediawiki::ClientConfig;
use wikisync_store::{Library, StoredCollection, SyncCheckpoint, SyncRunState, SyncRunStatus};

const DATABASE_NAME: &str = "library.sqlite3";
const RECENT_REVISION_LIMIT: u32 = 12;
const VERIFICATION_REVISION_LIMIT: u32 = 100;

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
    verification: VerificationState,
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
            verification: VerificationState::NotRun,
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
            }
            Message::CollectionNameChanged(value) => self.collection_form.name = value,
            Message::LanguageChanged(value) => self.collection_form.language_code = value,
            Message::EndpointChanged(value) => self.collection_form.api_endpoint = value,
            Message::CreateCollection => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let request = CreateCollectionRequest {
                    library_path: path.clone(),
                    name: self.collection_form.name.trim().to_owned(),
                    language_code: self.collection_form.language_code.trim().to_owned(),
                    api_endpoint: self.collection_form.api_endpoint.trim().to_owned(),
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
                        self.notice = Some(Notice::success(
                            "Empty collection configuration saved. Scope selection and synchronization controls are not available in this GUI slice yet.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::VerifyRecent => {
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
                list = list.push(collection_row(collection));
            }
        }

        let create_enabled = !self.is_busy()
            && !self.collection_form.name.trim().is_empty()
            && !self.collection_form.language_code.trim().is_empty()
            && !self.collection_form.api_endpoint.trim().is_empty();
        let form = column![
            text("Create an empty collection").size(23),
            text("This records collection configuration only. Network discovery and capture remain the responsibility of the shared sync worker."),
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
            button("Create collection")
                .on_press_maybe(create_enabled.then_some(Message::CreateCollection)),
        ]
        .spacing(10);

        column![list, horizontal_rule(1), form].spacing(18).into()
    }

    fn sync_view<'a>(&self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let mut content = column![
            text("Synchronization").size(30),
            text("Durable progress reported by the shared store. The GUI does not issue network requests; a sync worker or future daemon owns that work."),
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
                text("Reading and hash-verifying recent objects…").into()
            }
            VerificationState::Complete(report) => column![
                text(format!(
                    "Verified {} unique content objects referenced by {} recent revisions.",
                    report.verified_objects, report.revisions_checked
                )),
                text(if report.capped {
                    "The check reached its limit of the 100 most recent revisions; it does not represent whole-library coverage."
                } else {
                    "Every object referenced by the revisions returned for this bounded check was checked."
                }),
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
            button("Verify recent content objects").on_press_maybe(
                (!self.is_busy()
                    && !matches!(self.verification, VerificationState::Running))
                    .then_some(Message::VerifyRecent),
            ),
            result,
            text("A full manifest-chain verifier is not yet exposed by the shared integrity service. This action is intentionally bounded and does not claim whole-library coverage.").size(13),
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
    CreateCollection,
    CollectionCreated(ScopedResult<DashboardSnapshot>),
    VerifyRecent,
    VerificationFinished(ScopedResult<VerificationReport>),
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

#[derive(Clone, Debug, Default)]
struct CollectionForm {
    name: String,
    language_code: String,
    api_endpoint: String,
}

#[derive(Clone, Debug)]
struct CreateCollectionRequest {
    library_path: PathBuf,
    name: String,
    language_code: String,
    api_endpoint: String,
}

#[derive(Clone, Debug)]
enum VerificationState {
    NotRun,
    Running,
    Complete(VerificationReport),
    Failed(String),
}

#[derive(Clone, Debug)]
struct VerificationReport {
    verified_objects: usize,
    revisions_checked: usize,
    capped: bool,
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

fn verification_task(key: RequestKey) -> Task<Message> {
    Task::perform(
        async move {
            let result = verify_recent_objects(key.path.clone()).await;
            ScopedResult { key, result }
        },
        Message::VerificationFinished,
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
    create_collection_now(&request)
}

fn create_collection_now(request: &CreateCollectionRequest) -> Result<DashboardSnapshot, String> {
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
    library
        .create_explicit_collection(wiki_id, &request.name)
        .map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn verify_recent_objects(path: PathBuf) -> Result<VerificationReport, String> {
    let library = Library::open(path).map_err(|error| error.to_string())?;
    let revisions = library
        .recent_revisions(VERIFICATION_REVISION_LIMIT)
        .map_err(|error| error.to_string())?;
    let revisions_checked = revisions.len();
    let capped = revisions.len() == VERIFICATION_REVISION_LIMIT as usize;
    let mut ids = revisions
        .into_iter()
        .map(|(_, revision)| revision.content_object_id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    for id in &ids {
        library
            .read_object(*id)
            .map_err(|error| error.to_string())?;
    }
    Ok(VerificationReport {
        verified_objects: ids.len(),
        revisions_checked,
        capped,
    })
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

fn metric<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    container(column![text(value).size(25), text(label).size(13)].spacing(3))
        .padding(12)
        .width(Length::Fill)
        .into()
}

fn collection_row(collection: &StoredCollection) -> Element<'_, Message> {
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
    use wikisync_core::{PageId, PageTitle, RevisionId};
    use wikisync_store::CurrentRevisionCapture;

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
        assert_eq!(created.schema_version, 5);
        assert!(root.join(DATABASE_NAME).is_file());

        let reopened = load_library_snapshot(&root, false).expect("reopen library");
        assert!(reopened.collections.is_empty());
    }

    #[test]
    fn collection_form_uses_the_store_collection_service() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        load_library_snapshot(temporary.path(), true).expect("create library");

        let snapshot = create_collection_now(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Reference".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: "https://example.invalid/w/api.php".to_owned(),
        })
        .expect("create collection");

        assert_eq!(snapshot.collections.len(), 1);
        assert_eq!(snapshot.collections[0].name, "Reference");
        assert_eq!(snapshot.collections[0].page_count, 0);
    }

    #[test]
    fn invalid_endpoint_is_rejected_before_collection_mutation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        load_library_snapshot(temporary.path(), true).expect("create library");

        let error = create_collection_now(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Unsafe".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: "http://example.com/w/api.php".to_owned(),
        })
        .expect_err("remote plain HTTP endpoint must be rejected");

        assert!(error.contains("HTTPS"));
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
}
