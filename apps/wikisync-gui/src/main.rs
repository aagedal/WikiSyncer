use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

mod dump;

use iced::widget::{
    Space, button, checkbox, column, container, horizontal_rule, progress_bar, row, scrollable,
    text, text_input,
};
use iced::{Alignment, Element, Length, Task, Theme};
use wikisync_core::{
    CollectionBudget, CollectionId, CollectionRemovalPolicy, CollectionRule, HistoryPolicy,
    ImagePolicy, PageTitle, ThumbnailPolicy, UnixTimestamp, WikiId,
};
use wikisync_integrity::{
    MAX_TRUSTED_HEAD_BYTES, ManifestSigningKey, TrustedManifestHead, VerificationFindingKind,
    VerificationOptions, VerificationReport, VerificationScope, sign_current_manifest_head,
    verify_library, verify_library_against_trusted_head,
};
use wikisync_mediawiki::ClientConfig;
use wikisync_store::{
    CollectionSchedule, DumpImportStatus, Library, NetworkTransferPolicy, PurgeJournalState,
    PurgePreview, ScheduleCadence, StoredCollection, StoredCollectionConfiguration, StoredWiki,
    SyncCheckpoint, SyncRunState, SyncRunStatus,
};
use wikisync_sync::{
    CategoryPreviewLimits, CollectionSelectionPreview, bootstrap_collection, parse_title_list,
    preview_collection_rule, reconcile_collection_heads,
};
use wikisync_web::ReaderHandle;
use wikisyncd::{
    ApplicationHandler, CollectionAdministration, CollectionAdministrationOutcome, CollectionDraft,
    CollectionPurgeOutcome, CollectionPurgeRequest, MeteredNetworkState, Mutation,
    OperationControl, RequestHandler, SourceAdministration, SourceAdministrationOutcome,
    WriterAccess, WriterLease, administer_collection_direct, administer_source_direct,
    application_user_agent, bootstrap_collection_from_current_dump_direct_async,
    collection_purge_mutation, decode_collection_purge_outcome, detect_metered_network,
    next_occurrence_after, set_collection_schedule_mutation, set_network_transfer_policy_mutation,
};

use dump::{DumpBootstrapForm, DumpBootstrapPreview, INDEPENDENT_ANCHOR_NOTICE};

const DATABASE_NAME: &str = "library.sqlite3";
const RECENT_REVISION_LIMIT: u32 = 12;
const MAX_SIGNING_KEY_BYTES: u64 = 16 * 1024;

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
    collection_editor: Option<CollectionEditor>,
    remove_confirmation: Option<CollectionId>,
    purge_dialog: Option<CollectionPurgeDialog>,
    schedule_editor: Option<ScheduleEditor>,
    network_policy_editor: NetworkPolicyEditor,
    dump_bootstrap_form: DumpBootstrapForm,
    dump_bootstrap_preview: Option<DumpBootstrapPreview>,
    selection_preview: Option<CollectionSelectionPreview>,
    verification: VerificationState,
    signing_key_path: String,
    trusted_head_path: String,
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
            collection_editor: None,
            remove_confirmation: None,
            purge_dialog: None,
            schedule_editor: None,
            network_policy_editor: NetworkPolicyEditor::default(),
            dump_bootstrap_form: DumpBootstrapForm::default(),
            dump_bootstrap_preview: None,
            selection_preview: None,
            verification: VerificationState::NotRun,
            signing_key_path: String::new(),
            trusted_head_path: String::new(),
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
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
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
                self.dump_bootstrap_preview = None;
                self.purge_dialog = None;
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
            Message::CreateRemovalPolicyChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.removal_policy = value;
                }
            }
            Message::CreateImageModeChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.image_mode = value;
                }
            }
            Message::CreateThumbnailEdgeChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.thumbnail_max_edge_pixels = value;
                }
            }
            Message::CreateThumbnailCountChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.thumbnail_max_images_per_revision = value;
                }
            }
            Message::CreateThumbnailBytesChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.thumbnail_max_bytes_per_image = value;
                }
            }
            Message::CreateScheduleModeChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.schedule_mode = value;
                }
            }
            Message::CreateScheduleValueChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.schedule_value = value;
                }
            }
            Message::CreateScheduleJitterChanged(value) => {
                if !self.is_busy() {
                    self.collection_form.schedule_jitter_minutes = value;
                }
            }
            Message::CreateSchedulePaused(value) => {
                if !self.is_busy() {
                    self.collection_form.schedule_paused = value;
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
                    network_policy: self
                        .snapshot
                        .as_ref()
                        .map_or_else(NetworkTransferPolicy::default, |snapshot| {
                            snapshot.network_policy
                        }),
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
                    removal_policy: self.collection_form.removal_policy,
                    image_policy: match self.collection_form.image_policy() {
                        Ok(policy) => policy,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                    schedule: match self.collection_form.schedule() {
                        Ok(schedule) => schedule,
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
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
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
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
                        self.snapshot = Some(snapshot);
                        self.notice = Some(Notice::success(
                            "Collection update completed; every discovered intermediate revision is durable.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::EditCollection(collection_id) => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(snapshot) = &self.snapshot else {
                    return Task::none();
                };
                let Some(configuration) = snapshot
                    .collection_configurations
                    .iter()
                    .find(|configuration| configuration.collection_id == collection_id)
                else {
                    self.notice = Some(Notice::error("Collection configuration is unavailable."));
                    return Task::none();
                };
                let Some(wiki) = snapshot
                    .wikis
                    .iter()
                    .find(|wiki| wiki.wiki_id == configuration.wiki_id)
                else {
                    self.notice = Some(Notice::error("Collection source is unavailable."));
                    return Task::none();
                };
                let schedule = snapshot
                    .schedules
                    .iter()
                    .find(|schedule| schedule.collection_id == collection_id)
                    .copied();
                self.collection_editor = Some(CollectionEditor {
                    collection_id,
                    expected_generation: configuration.generation,
                    form: CollectionForm::from_configuration(configuration, wiki, schedule),
                    preview: None,
                });
                self.schedule_editor = None;
                self.remove_confirmation = None;
                self.notice = Some(Notice::success(
                    "Edit loaded. Preview the complete replacement scope before saving.",
                ));
            }
            Message::CancelCollectionEdit => {
                if !self.is_busy() {
                    self.collection_editor = None;
                }
            }
            Message::EditCollectionNameChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.name = value;
                    }
                }
            }
            Message::EditSelectionModeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.selection_mode = value;
                        editor.preview = None;
                    }
                }
            }
            Message::EditSelectionChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.selection = value;
                        editor.preview = None;
                    }
                }
            }
            Message::EditCategoryDepthChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.category_depth = value;
                        editor.preview = None;
                    }
                }
            }
            Message::EditHistoryModeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.history_mode = value;
                    }
                }
            }
            Message::EditHistoryValueChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.history_value = value;
                    }
                }
            }
            Message::EditMaximumPagesChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.maximum_pages = value;
                    }
                }
            }
            Message::EditMaximumBytesChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.maximum_bytes = value;
                    }
                }
            }
            Message::EditRemovalPolicyChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.removal_policy = value;
                    }
                }
            }
            Message::EditImageModeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.image_mode = value;
                    }
                }
            }
            Message::EditThumbnailEdgeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.thumbnail_max_edge_pixels = value;
                    }
                }
            }
            Message::EditThumbnailCountChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.thumbnail_max_images_per_revision = value;
                    }
                }
            }
            Message::EditThumbnailBytesChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.thumbnail_max_bytes_per_image = value;
                    }
                }
            }
            Message::EditCollectionScheduleModeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.schedule_mode = value;
                    }
                }
            }
            Message::EditCollectionScheduleValueChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.schedule_value = value;
                    }
                }
            }
            Message::EditCollectionScheduleJitterChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.schedule_jitter_minutes = value;
                    }
                }
            }
            Message::EditCollectionSchedulePaused(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.collection_editor {
                        editor.form.schedule_paused = value;
                    }
                }
            }
            Message::PreviewCollectionEdit => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(editor) = &self.collection_editor else {
                    return Task::none();
                };
                let request = PreviewCollectionRequest {
                    api_endpoint: editor.form.api_endpoint.clone(),
                    network_policy: self
                        .snapshot
                        .as_ref()
                        .map_or_else(NetworkTransferPolicy::default, |snapshot| {
                            snapshot.network_policy
                        }),
                    rule: match editor.form.rule() {
                        Ok(rule) => rule,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                };
                let expected_generation = match collection_generation(
                    Path::new(&self.library_path),
                    editor.collection_id,
                ) {
                    Ok(generation) => generation,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                if let Some(editor) = &mut self.collection_editor {
                    editor.expected_generation = expected_generation;
                }
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return edit_preview_task(key, request);
            }
            Message::CollectionEditPreviewed(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(preview) => {
                        let pages = preview.members.len();
                        let missing = preview.missing_titles.len();
                        if let Some(editor) = &mut self.collection_editor {
                            editor.preview = Some(preview);
                        }
                        self.notice = Some(Notice::success(format!(
                            "Edit preview complete: {pages} resolved page(s), {missing} missing title(s). No configuration has changed yet."
                        )));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::SaveCollectionEdit => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(editor) = &self.collection_editor else {
                    return Task::none();
                };
                let Some(preview) = editor.preview.clone() else {
                    self.notice = Some(Notice::error("Preview the complete edit before saving."));
                    return Task::none();
                };
                let Some(configuration) = self.snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .collection_configurations
                        .iter()
                        .find(|configuration| configuration.collection_id == editor.collection_id)
                }) else {
                    self.notice = Some(Notice::error("Collection configuration is unavailable."));
                    return Task::none();
                };
                let request = EditCollectionRequest {
                    library_path: PathBuf::from(&self.library_path),
                    collection_id: editor.collection_id,
                    expected_generation: editor.expected_generation,
                    wiki_id: configuration.wiki_id,
                    name: editor.form.name.trim().to_owned(),
                    preview,
                    history_policy: match editor.form.history_policy() {
                        Ok(value) => value,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                    budget: match editor.form.budget() {
                        Ok(value) => value,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                    removal_policy: editor.form.removal_policy,
                    image_policy: match editor.form.image_policy() {
                        Ok(value) => value,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                    schedule: match editor.form.schedule() {
                        Ok(value) => value,
                        Err(error) => {
                            self.notice = Some(Notice::error(error));
                            return Task::none();
                        }
                    },
                };
                self.notice = None;
                let key = self.begin_request(request.library_path.clone());
                return edit_collection_task(key, request);
            }
            Message::CollectionEditSaved(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.collection_editor = None;
                        self.notice = Some(Notice::success(
                            "The previewed collection configuration and schedule were saved. Use Update to capture newly selected revisions.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::PreviewRemoveCollection(collection_id) => {
                if !self.is_busy() {
                    self.collection_editor = None;
                    self.schedule_editor = None;
                    self.remove_confirmation = Some(collection_id);
                    self.purge_dialog = None;
                }
            }
            Message::CancelRemoveCollection => {
                if !self.is_busy() {
                    self.remove_confirmation = None;
                }
            }
            Message::ConfirmRemoveCollection => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(collection_id) = self.remove_confirmation else {
                    return Task::none();
                };
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return remove_collection_task(key, path, collection_id);
            }
            Message::CollectionRemoved(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.remove_confirmation = None;
                        self.notice = Some(Notice::success(
                            "Tracking stopped. Captured revisions, historical runs, manifests, and integrity evidence were retained; no article data was purged.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::OpenCollectionPurge(collection_id) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.remove_confirmation = None;
                self.collection_editor = None;
                self.schedule_editor = None;
                self.notice = None;
                self.purge_dialog = Some(CollectionPurgeDialog::new(collection_id));
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return purge_preview_task(key, path, collection_id);
            }
            Message::RefreshCollectionPurgePreview => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(dialog) = self.purge_dialog.as_mut() else {
                    return Task::none();
                };
                dialog.clear_confirmations();
                dialog.preview = None;
                self.notice = None;
                let collection_id = dialog.collection_id;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return purge_preview_task(key, path, collection_id);
            }
            Message::CollectionPurgePreviewed(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(preview) => {
                        let Some(dialog) = self.purge_dialog.as_mut() else {
                            return Task::none();
                        };
                        if dialog.collection_id != preview.collection_id {
                            return Task::none();
                        }
                        dialog.install_preview(preview);
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::CollectionPurgeNameChanged(value) => {
                if !self.is_busy()
                    && let Some(dialog) = self.purge_dialog.as_mut()
                {
                    dialog.typed_name = value;
                }
            }
            Message::CollectionPurgeFingerprintChanged(value) => {
                if !self.is_busy()
                    && let Some(dialog) = self.purge_dialog.as_mut()
                {
                    dialog.typed_fingerprint = value;
                }
            }
            Message::CollectionPurgePayloadAcknowledged(value) => {
                if !self.is_busy()
                    && let Some(dialog) = self.purge_dialog.as_mut()
                {
                    dialog.payload_only_acknowledged = value;
                }
            }
            Message::CollectionPurgeExternalCopiesAcknowledged(value) => {
                if !self.is_busy()
                    && let Some(dialog) = self.purge_dialog.as_mut()
                {
                    dialog.external_copies_acknowledged = value;
                }
            }
            Message::CancelCollectionPurge => {
                if !self.is_busy() {
                    self.purge_dialog = None;
                }
            }
            Message::ConfirmCollectionPurge => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(request) = self
                    .purge_dialog
                    .as_ref()
                    .and_then(CollectionPurgeDialog::confirmed_request)
                else {
                    self.notice = Some(Notice::error(
                        "The exact name, exact fingerprint, and both acknowledgements are required.",
                    ));
                    return Task::none();
                };
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return purge_collection_task(key, path, request);
            }
            Message::CollectionPurged(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(result) => {
                        self.snapshot = Some(result.snapshot);
                        self.purge_dialog = None;
                        self.notice = Some(Notice::success(purge_outcome_summary(&result.outcome)));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::EditSchedule(collection_id) => {
                if self.is_busy() {
                    return Task::none();
                }
                let schedule = self.snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .schedules
                        .iter()
                        .find(|schedule| schedule.collection_id == collection_id)
                        .copied()
                });
                self.schedule_editor = Some(ScheduleEditor::from_schedule(
                    collection_id,
                    schedule.unwrap_or(CollectionSchedule {
                        collection_id,
                        cadence: ScheduleCadence::Manual,
                        jitter_seconds: 0,
                        paused: false,
                        next_run_at: None,
                        last_started_at: None,
                    }),
                ));
            }
            Message::EditScheduleModeChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.schedule_editor {
                        editor.mode = value;
                    }
                }
            }
            Message::EditScheduleValueChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.schedule_editor {
                        editor.value = value;
                    }
                }
            }
            Message::EditScheduleJitterChanged(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.schedule_editor {
                        editor.jitter_minutes = value;
                    }
                }
            }
            Message::EditSchedulePaused(value) => {
                if !self.is_busy() {
                    if let Some(editor) = &mut self.schedule_editor {
                        editor.paused = value;
                    }
                }
            }
            Message::SaveSchedule => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(editor) = &self.schedule_editor else {
                    return Task::none();
                };
                let schedule = match editor.settings() {
                    Ok(schedule) => schedule,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                let collection_id = editor.collection_id;
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return save_schedule_task(key, path, collection_id, schedule);
            }
            Message::ScheduleSaved(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
                        self.snapshot = Some(snapshot);
                        self.schedule_editor = None;
                        self.notice = Some(Notice::success("Schedule saved."));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::NetworkConcurrencyChanged(value) => {
                if !self.is_busy() {
                    self.network_policy_editor.max_concurrent_requests = value;
                }
            }
            Message::NetworkRateChanged(value) => {
                if !self.is_busy() {
                    self.network_policy_editor.max_download_bytes_per_second = value;
                }
            }
            Message::AvoidMeteredNetworksChanged(value) => {
                if !self.is_busy() {
                    self.network_policy_editor.avoid_metered_networks = value;
                }
            }
            Message::SaveNetworkPolicy => {
                if self.is_busy() {
                    return Task::none();
                }
                let policy = match self.network_policy_editor.policy() {
                    Ok(policy) => policy,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return save_network_policy_task(key, path, policy);
            }
            Message::NetworkPolicySaved(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
                        self.snapshot = Some(snapshot);
                        self.notice = Some(Notice::success("Network transfer policy saved."));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::DumpCollectionChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.collection_id = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpIndexUrlChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.trusted_index_url = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpIndexDigestChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.trusted_index_digest = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpExpectedDatabaseChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.expected_database = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxIndexBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_index_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxArtifactBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_artifact_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxTotalArtifactBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_total_artifact_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxArtifactsChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_artifacts = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxElapsedSecondsChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_elapsed_seconds = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxCompressedBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_compressed_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxDecompressedBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_decompressed_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxPagesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_pages = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxPageXmlBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_page_xml_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::DumpMaxTextBytesChanged(value) => {
                if !self.is_busy() {
                    self.dump_bootstrap_form.max_text_bytes = value;
                    self.dump_bootstrap_preview = None;
                }
            }
            Message::PreviewDumpBootstrap => {
                if self.is_busy() {
                    return Task::none();
                }
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return dump_preview_task(key, path, self.dump_bootstrap_form.clone());
            }
            Message::DumpBootstrapPreviewed(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(preview) => {
                        self.notice = Some(Notice::success(
                            "Dump bootstrap preview is ready. Confirm the independent trust identity, scope, limits, and hard budgets before starting.",
                        ));
                        self.dump_bootstrap_preview = Some(preview);
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::StartDumpBootstrap => {
                if self.is_busy() {
                    return Task::none();
                }
                let Some(preview) = self.dump_bootstrap_preview.clone() else {
                    self.notice = Some(Notice::error(
                        "Preview the authenticated dump bootstrap before starting it.",
                    ));
                    return Task::none();
                };
                self.notice = None;
                let path = PathBuf::from(&self.library_path);
                let key = self.begin_request(path.clone());
                return dump_bootstrap_task(key, path, preview);
            }
            Message::DumpBootstrapFinished(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                match completion.result {
                    Ok(snapshot) => {
                        self.network_policy_editor =
                            NetworkPolicyEditor::from_policy(snapshot.network_policy);
                        self.snapshot = Some(snapshot);
                        self.dump_bootstrap_preview = None;
                        self.notice = Some(Notice::success(
                            "Authenticated current-dump bootstrap completed. Its durable import status and checkpoint are shown below.",
                        ));
                    }
                    Err(error) => self.notice = Some(Notice::error(error)),
                }
            }
            Message::SigningKeyPathChanged(value) => {
                if !self.is_busy() {
                    self.signing_key_path = value;
                }
            }
            Message::TrustedHeadPathChanged(value) => {
                if !self.is_busy() {
                    self.trusted_head_path = value;
                }
            }
            Message::GenerateSigningKey => {
                if self.is_busy() {
                    return Task::none();
                }
                let key_path = match explicit_artifact_path(
                    &self.library_path,
                    &self.signing_key_path,
                    "Signing key",
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return generate_signing_key_task(key, key_path);
            }
            Message::SigningKeyGenerated(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                self.notice = Some(match completion.result {
                    Ok(summary) => Notice::success(summary),
                    Err(error) => Notice::error(error),
                });
            }
            Message::ValidateSigningKey => {
                if self.is_busy() {
                    return Task::none();
                }
                let key_path = match explicit_artifact_path(
                    &self.library_path,
                    &self.signing_key_path,
                    "Signing key",
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return validate_signing_key_task(key, key_path);
            }
            Message::SigningKeyValidated(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                self.notice = Some(match completion.result {
                    Ok(summary) => Notice::success(summary),
                    Err(error) => Notice::error(error),
                });
            }
            Message::RefreshTrustedHead => {
                if self.is_busy() {
                    return Task::none();
                }
                let key_path = match explicit_artifact_path(
                    &self.library_path,
                    &self.signing_key_path,
                    "Signing key",
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                let trusted_head_path = match explicit_artifact_path(
                    &self.library_path,
                    &self.trusted_head_path,
                    "Trusted-head anchor",
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                self.verification = VerificationState::Running(VerificationKind::AnchorRefresh);
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return refresh_trusted_head_task(key, key_path, trusted_head_path);
            }
            Message::TrustedHeadRefreshed(completion) => {
                if !self.finish_request(&completion.key) {
                    return Task::none();
                }
                self.notice = Some(match completion.result {
                    Ok(result) => {
                        self.verification = VerificationState::Complete(result.report);
                        Notice::success(result.summary)
                    }
                    Err(error) => {
                        self.verification = VerificationState::Failed(error.clone());
                        Notice::error(error)
                    }
                });
            }
            Message::VerifyFull => {
                if self.is_busy() {
                    return Task::none();
                }
                self.verification = VerificationState::Running(VerificationKind::Local);
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return verification_task(key);
            }
            Message::VerifyTrustedHead => {
                if self.is_busy() {
                    return Task::none();
                }
                let trusted_head_path = match explicit_artifact_path(
                    &self.library_path,
                    &self.trusted_head_path,
                    "Trusted-head anchor",
                ) {
                    Ok(path) => path,
                    Err(error) => {
                        self.notice = Some(Notice::error(error));
                        return Task::none();
                    }
                };
                self.verification = VerificationState::Running(VerificationKind::TrustedHead);
                self.notice = None;
                let key = self.begin_request(PathBuf::from(&self.library_path));
                return trusted_verification_task(key, trusted_head_path);
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

    fn collections_view<'a>(&'a self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let mut list = column![text("Collections").size(30)].spacing(10);
        if snapshot.collections.is_empty() {
            list = list.push(text("No actively tracked collections. Create one below."));
        } else {
            for collection in &snapshot.collections {
                let schedule = snapshot
                    .schedules
                    .iter()
                    .find(|schedule| schedule.collection_id == collection.collection_id);
                let configuration = snapshot
                    .collection_configurations
                    .iter()
                    .find(|configuration| configuration.collection_id == collection.collection_id);
                list = list.push(collection_row(
                    collection,
                    configuration,
                    schedule,
                    !self.is_busy(),
                ));
            }
        }

        if !snapshot.tombstoned_collections.is_empty() {
            list = list.push(horizontal_rule(1));
            list = list.push(text("Stopped collections").size(23));
            list = list.push(text(
                "These tombstones retain history and audit evidence. Payload purge is a separate destructive operation and always starts with a read-only preview.",
            ));
            for collection in &snapshot.tombstoned_collections {
                list = list.push(tombstoned_collection_row(collection, !self.is_busy()));
            }
        }

        if let Some(editor) = &self.collection_editor {
            list = list.push(collection_edit_view(editor, !self.is_busy()));
        }

        if let Some(collection_id) = self.remove_confirmation {
            let name = snapshot
                .collections
                .iter()
                .find(|collection| collection.collection_id == collection_id)
                .map_or("this collection", |collection| collection.name.as_str());
            list = list.push(
                container(
                    column![
                        text(format!("Stop tracking {name}?")).size(20),
                        text("This non-destructive removal tombstones the collection and stops future membership resolution and synchronization. Every captured revision, historical sync run, manifest, and integrity record is retained. It does not reclaim article storage."),
                        row![
                            button("Cancel").on_press(Message::CancelRemoveCollection),
                            button("Confirm: stop tracking").on_press_maybe(
                                (!self.is_busy()).then_some(Message::ConfirmRemoveCollection)
                            ),
                        ]
                        .spacing(8),
                    ]
                    .spacing(9),
                )
                .padding(12),
            );
        }

        if let Some(dialog) = &self.purge_dialog {
            list = list.push(collection_purge_view(dialog, !self.is_busy()));
        }

        if let Some(editor) = &self.schedule_editor {
            let schedule_modes = row![
                schedule_button(
                    "Manual",
                    ScheduleMode::Manual,
                    editor.mode,
                    Message::EditScheduleModeChanged,
                ),
                schedule_button(
                    "Interval",
                    ScheduleMode::Interval,
                    editor.mode,
                    Message::EditScheduleModeChanged,
                ),
                schedule_button(
                    "Daily UTC",
                    ScheduleMode::DailyUtc,
                    editor.mode,
                    Message::EditScheduleModeChanged,
                ),
            ]
            .spacing(8);
            let value_hint = schedule_value_hint(editor.mode);
            list = list.push(
                container(
                    column![
                        text(format!("Schedule collection {}", editor.collection_id)).size(20),
                        schedule_modes,
                        text_input(value_hint, &editor.value)
                            .on_input(Message::EditScheduleValueChanged)
                            .padding(10),
                        text_input("Maximum jitter in minutes", &editor.jitter_minutes)
                            .on_input(Message::EditScheduleJitterChanged)
                            .padding(10),
                        checkbox("Pause automatic synchronization", editor.paused)
                            .on_toggle(Message::EditSchedulePaused),
                        button("Save schedule")
                            .on_press_maybe((!self.is_busy()).then_some(Message::SaveSchedule)),
                    ]
                    .spacing(9),
                )
                .padding(12),
            );
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
        let schedule_buttons = row![
            schedule_button(
                "Manual",
                ScheduleMode::Manual,
                self.collection_form.schedule_mode,
                Message::CreateScheduleModeChanged,
            ),
            schedule_button(
                "Interval",
                ScheduleMode::Interval,
                self.collection_form.schedule_mode,
                Message::CreateScheduleModeChanged,
            ),
            schedule_button(
                "Daily UTC",
                ScheduleMode::DailyUtc,
                self.collection_form.schedule_mode,
                Message::CreateScheduleModeChanged,
            ),
        ]
        .spacing(8);
        let image_buttons = row![
            image_policy_button(
                "No images",
                ImageMode::None,
                self.collection_form.image_mode,
                Message::CreateImageModeChanged,
            ),
            image_policy_button(
                "Bounded thumbnails",
                ImageMode::Thumbnails,
                self.collection_form.image_mode,
                Message::CreateImageModeChanged,
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
            text("When a dynamic rule no longer selects a page").size(17),
            row![
                removal_policy_button(
                    "Stop tracking; retain captured history",
                    CollectionRemovalPolicy::StopTrackingRetainHistory,
                    self.collection_form.removal_policy,
                    Message::CreateRemovalPolicyChanged,
                ),
                removal_policy_button(
                    "Keep tracking",
                    CollectionRemovalPolicy::KeepTracking,
                    self.collection_form.removal_policy,
                    Message::CreateRemovalPolicyChanged,
                ),
            ]
            .spacing(8),
            text("Referenced image capture").size(17),
            image_buttons,
            row![
                text_input(
                    "Maximum thumbnail edge (pixels)",
                    &self.collection_form.thumbnail_max_edge_pixels,
                )
                .on_input(Message::CreateThumbnailEdgeChanged)
                .padding(10),
                text_input(
                    "Maximum images per revision",
                    &self.collection_form.thumbnail_max_images_per_revision,
                )
                .on_input(Message::CreateThumbnailCountChanged)
                .padding(10),
                text_input(
                    "Maximum bytes per thumbnail",
                    &self.collection_form.thumbnail_max_bytes_per_image,
                )
                .on_input(Message::CreateThumbnailBytesChanged)
                .padding(10),
            ]
            .spacing(8),
            text(image_policy_form_summary(&self.collection_form)),
            text("Automatic synchronization schedule").size(17),
            schedule_buttons,
            text_input(
                schedule_value_hint(self.collection_form.schedule_mode),
                &self.collection_form.schedule_value,
            )
            .on_input(Message::CreateScheduleValueChanged)
            .padding(10),
            text_input(
                "Maximum jitter in minutes",
                &self.collection_form.schedule_jitter_minutes,
            )
            .on_input(Message::CreateScheduleJitterChanged)
            .padding(10),
            checkbox(
                "Create schedule paused",
                self.collection_form.schedule_paused,
            )
            .on_toggle(Message::CreateSchedulePaused),
            text("Stopping tracking never deletes already captured revisions. Keep tracking leaves departed dynamic members active until explicitly changed."),
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

    fn sync_view<'a>(&'a self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let rate_label = snapshot
            .network_policy
            .max_download_bytes_per_second()
            .map_or_else(
                || "unlimited".to_owned(),
                |bytes| format!("{}/s", format_bytes(bytes)),
            );
        let mut content = column![
            text("Synchronization").size(30),
            text("Create-and-sync and Update actions use the shared durable capture/reconciliation services. Interrupted page jobs resume without advancing their checkpoint prematurely."),
            container(
                column![
                    text("Network transfer policy").size(23),
                    text(format!(
                        "Active policy: up to {} concurrent request(s), byte rate {rate_label}, metered-network avoidance {}.",
                        snapshot.network_policy.max_concurrent_requests(),
                        if snapshot.network_policy.avoid_metered_networks() { "enabled" } else { "disabled" },
                    )),
                    row![
                        text_input(
                            "Maximum concurrent requests",
                            &self.network_policy_editor.max_concurrent_requests,
                        )
                        .on_input(Message::NetworkConcurrencyChanged)
                        .padding(10),
                        text_input(
                            "Maximum downloaded bytes/second (blank = unlimited)",
                            &self.network_policy_editor.max_download_bytes_per_second,
                        )
                        .on_input(Message::NetworkRateChanged)
                        .padding(10),
                    ]
                    .spacing(10),
                    checkbox(
                        "Avoid synchronization on connections reported as metered",
                        self.network_policy_editor.avoid_metered_networks,
                    )
                    .on_toggle(Message::AvoidMeteredNetworksChanged),
                    text("Linux uses NetworkManager's metered state when available. Unsupported or indeterminate detection is reported as unknown and does not silently block synchronization.").size(13),
                    button("Save network policy")
                        .on_press_maybe((!self.is_busy()).then_some(Message::SaveNetworkPolicy)),
                ]
                .spacing(9),
            )
            .padding(12),
            self.dump_bootstrap_panel(),
        ]
        .spacing(10);

        content = content.push(text("Durable dump-import progress").size(23));
        if snapshot.dump_imports.is_empty() {
            content = content.push(text("No authenticated current-dump imports recorded."));
        } else {
            for import in &snapshot.dump_imports {
                let mut card = column![
                    row![
                        text(format!(
                            "Dump import #{} · collection {} · run #{}",
                            import.import_id, import.collection_id, import.run_id
                        ))
                        .size(19),
                        Space::new(Length::Fill, Length::Shrink),
                        text(import.state.as_str()),
                    ],
                    text(format!(
                        "{} pages scanned · {} selected pages imported · {} canonical bytes · attempt {}{}",
                        import.pages_scanned,
                        import.imported_pages,
                        format_bytes(import.imported_canonical_bytes),
                        import.attempt_count,
                        if import.retryable { " · retryable" } else { "" },
                    )),
                    text(format!(
                        "Authenticated index {} · compressed artifact set {}",
                        import.dump_digest,
                        format_bytes(import.dump_compressed_bytes),
                    ))
                    .size(13),
                ]
                .spacing(7);
                if let Some(error) = &import.latest_error {
                    card = card.push(text(format!(
                        "Last import error [{}]: {}",
                        error.code, error.message
                    )));
                }
                content = content.push(container(card).padding(12));
            }
        }

        content = content.push(horizontal_rule(1));

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

    fn dump_bootstrap_panel(&self) -> Element<'_, Message> {
        let form = &self.dump_bootstrap_form;
        let preview: Element<'_, Message> = self.dump_bootstrap_preview.as_ref().map_or_else(
            || text("No dump bootstrap preview is active.").into(),
            |preview| {
                let page_budget = preview.budget.maximum_pages().map_or_else(
                    || "unlimited".to_owned(),
                    |value| value.get().to_string(),
                );
                let byte_budget = preview.budget.maximum_bytes().map_or_else(
                    || "unlimited".to_owned(),
                    |value| format_bytes(value.get()),
                );
                let byte_rate = preview.max_download_bytes_per_second.map_or_else(
                    || "unlimited".to_owned(),
                    |value| format!("{}/s", format_bytes(value)),
                );
                container(
                    column![
                        text("Confirmed preview").size(20),
                        text(format!(
                            "Source: {} ({}, wiki {})",
                            preview.source_endpoint, preview.language_code, preview.wiki_id
                        )),
                        text(format!(
                            "Trust identity: BLAKE3 {} for {} (expected database {})",
                            preview.draft.trusted_index_digest.to_hex(),
                            preview.draft.trusted_index_url,
                            preview.draft.expected_database,
                        )),
                        text(format!(
                            "Scope: collection {} “{}”, generation {}, {} resolved pages",
                            preview.draft.collection_id,
                            preview.collection_name,
                            preview.collection_generation,
                            preview.resolved_pages,
                        )),
                        text(format!(
                            "Hard collection budgets: pages {page_budget}; canonical bytes {byte_budget}."
                        )),
                        text(format!(
                            "Transfer/storage ceilings: index {}, each artifact {}, all cached artifacts {}, {} artifacts, {} seconds; private cache {}.",
                            format_bytes(preview.draft.acquisition_limits.max_index_bytes as u64),
                            format_bytes(preview.draft.acquisition_limits.max_artifact_bytes),
                            format_bytes(preview.draft.acquisition_limits.max_total_artifact_bytes),
                            preview.draft.acquisition_limits.max_artifacts,
                            preview.draft.acquisition_limits.max_elapsed.as_secs(),
                            preview.cache_directory,
                        )),
                        text(format!(
                            "Durable network policy: {} concurrent request(s), byte rate {byte_rate}, metered-network avoidance {}.",
                            preview.max_concurrent_requests,
                            if preview.avoid_metered_networks { "enabled" } else { "disabled" },
                        )),
                        text(format!(
                            "Parser ceilings: compressed {}, decompressed {}, {} pages, {} XML/page, {} revision text.",
                            format_bytes(preview.draft.parser_limits.max_compressed_bytes),
                            format_bytes(preview.draft.parser_limits.max_decompressed_bytes),
                            preview.draft.parser_limits.max_pages,
                            format_bytes(preview.draft.parser_limits.max_page_xml_bytes),
                            format_bytes(preview.draft.parser_limits.max_text_bytes as u64),
                        )),
                        button("Confirm and start authenticated dump bootstrap").on_press_maybe(
                            (!self.is_busy()).then_some(Message::StartDumpBootstrap)
                        ),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .into()
            },
        );

        container(
            column![
                text("Authenticated current-dump bootstrap").size(23),
                text("Use a current-page dump only for an already resolved current-and-future collection. Previewing is read-only; starting is a separate single-writer action."),
                text(INDEPENDENT_ANCHOR_NOTICE).size(13),
                row![
                    text_input("Collection ID", &form.collection_id)
                        .on_input(Message::DumpCollectionChanged)
                        .padding(10),
                    text_input("Expected database (for example: enwiki)", &form.expected_database)
                        .on_input(Message::DumpExpectedDatabaseChanged)
                        .padding(10),
                ]
                .spacing(10),
                text_input("Trusted dump index URL", &form.trusted_index_url)
                    .on_input(Message::DumpIndexUrlChanged)
                    .padding(10),
                text_input("Independently retained BLAKE3 index digest (64 hex digits)", &form.trusted_index_digest)
                    .on_input(Message::DumpIndexDigestChanged)
                    .padding(10),
                text("Transfer and cached-artifact storage limits").size(17),
                row![
                    text_input("Maximum index bytes", &form.max_index_bytes)
                        .on_input(Message::DumpMaxIndexBytesChanged)
                        .padding(10),
                    text_input("Maximum bytes per artifact", &form.max_artifact_bytes)
                        .on_input(Message::DumpMaxArtifactBytesChanged)
                        .padding(10),
                    text_input("Maximum total artifact bytes", &form.max_total_artifact_bytes)
                        .on_input(Message::DumpMaxTotalArtifactBytesChanged)
                        .padding(10),
                ]
                .spacing(8),
                row![
                    text_input("Maximum artifact count", &form.max_artifacts)
                        .on_input(Message::DumpMaxArtifactsChanged)
                        .padding(10),
                    text_input("Maximum acquisition seconds", &form.max_elapsed_seconds)
                        .on_input(Message::DumpMaxElapsedSecondsChanged)
                        .padding(10),
                ]
                .spacing(8),
                text("Streaming parser limits").size(17),
                row![
                    text_input("Maximum compressed bytes", &form.max_compressed_bytes)
                        .on_input(Message::DumpMaxCompressedBytesChanged)
                        .padding(10),
                    text_input("Maximum decompressed bytes", &form.max_decompressed_bytes)
                        .on_input(Message::DumpMaxDecompressedBytesChanged)
                        .padding(10),
                    text_input("Maximum scanned pages", &form.max_pages)
                        .on_input(Message::DumpMaxPagesChanged)
                        .padding(10),
                ]
                .spacing(8),
                row![
                    text_input("Maximum XML bytes per page", &form.max_page_xml_bytes)
                        .on_input(Message::DumpMaxPageXmlBytesChanged)
                        .padding(10),
                    text_input("Maximum revision text bytes", &form.max_text_bytes)
                        .on_input(Message::DumpMaxTextBytesChanged)
                        .padding(10),
                ]
                .spacing(8),
                button("Preview authenticated dump bootstrap")
                    .on_press_maybe((!self.is_busy()).then_some(Message::PreviewDumpBootstrap)),
                preview,
            ]
            .spacing(9),
        )
        .padding(12)
        .into()
    }

    fn integrity_view<'a>(&'a self, snapshot: &'a DashboardSnapshot) -> Element<'a, Message> {
        let result: Element<'_, Message> = match &self.verification {
            VerificationState::NotRun => {
                text("No read verification has run in this session.").into()
            }
            VerificationState::Running(kind) => text(match kind {
                VerificationKind::Local => {
                    "Reading and hash-verifying the full library, manifest chain, and metadata references…"
                }
                VerificationKind::TrustedHead => {
                    "Running full verification and comparing the manifest-chain head with the external Ed25519 anchor…"
                }
                VerificationKind::AnchorRefresh => {
                    "Signing the observed head in memory, then running full authenticated verification before publishing the external anchor…"
                }
            })
            .into(),
            VerificationState::Complete(report) => verification_report_view(report),
            VerificationState::Failed(error) => {
                text(format!("Verification stopped: {error}")).into()
            }
        };

        let controls_enabled =
            !self.is_busy() && !matches!(self.verification, VerificationState::Running(_));

        column![
            text("Integrity").size(30),
            text("WikiSyncer can prove that captured canonical bytes, manifests, and internal references remain consistent with their recorded identities. It cannot prove that an upstream statement was true, unbiased, complete, or still available."),
            row![
                metric("Schema version", snapshot.schema_version.to_string()),
                metric(
                    "Unique objects in overview sample",
                    snapshot.recent_unique_object_count.to_string()
                ),
                metric("Files on disk", storage_files_label(&snapshot.storage_usage)),
            ]
            .spacing(12),
            row![
                button("Verify full library")
                    .on_press_maybe(controls_enabled.then_some(Message::VerifyFull)),
                button("Verify against external anchor")
                    .on_press_maybe(controls_enabled.then_some(Message::VerifyTrustedHead)),
            ]
            .spacing(8),
            result,
            horizontal_rule(1),
            text("External Ed25519 trust anchor").size(23),
            text("Choose explicit paths outside the library. WikiSyncer never silently stores the only trusted-head anchor beside the library it is meant to detect replacement or rollback of."),
            text("Private signing key (PKCS#8)").size(14),
            text_input("/separate/private/location/wikisync-signing-key.pk8", &self.signing_key_path)
                .on_input(Message::SigningKeyPathChanged)
                .padding(10),
            row![
                button("Generate new key")
                    .on_press_maybe(controls_enabled.then_some(Message::GenerateSigningKey)),
                button("Validate existing key")
                    .on_press_maybe(controls_enabled.then_some(Message::ValidateSigningKey)),
            ]
            .spacing(8),
            text("Generation never overwrites an existing file and creates a private 0600 file on Unix. Import/use is explicit: enter an existing protected PKCS#8 path, validate it, then refresh the anchor. Back up the key separately; the anchor contains only its public key.").size(13),
            text("Trusted-head anchor (canonical JSON)").size(14),
            text_input("/separate/trusted/location/wikisync-trusted-head.json", &self.trusted_head_path)
                .on_input(Message::TrustedHeadPathChanged)
                .padding(10),
            button("Full verify, sign, and refresh anchor")
                .on_press_maybe(controls_enabled.then_some(Message::RefreshTrustedHead)),
            text("Refresh after the library advances. Keep independent copies or history for the anchor: replacing both the library and its only anchor defeats comparison. A valid older anchor is intentionally reported as a mismatch, not silently accepted.").size(13),
            container(
                column![
                    text("Rotation and recovery").size(18),
                    text("To rotate, generate a key at a new path, retain the previous anchor for audit/recovery, then refresh the anchor with the new key. If the key or anchor is lost, first establish the expected library state from an independent backup or other trusted evidence; only then create a replacement. A fresh self-signed anchor alone cannot turn an uncertain library into trusted truth."),
                ]
                .spacing(7),
            )
            .padding(12),
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
    CreateRemovalPolicyChanged(CollectionRemovalPolicy),
    CreateImageModeChanged(ImageMode),
    CreateThumbnailEdgeChanged(String),
    CreateThumbnailCountChanged(String),
    CreateThumbnailBytesChanged(String),
    CreateScheduleModeChanged(ScheduleMode),
    CreateScheduleValueChanged(String),
    CreateScheduleJitterChanged(String),
    CreateSchedulePaused(bool),
    PreviewCollection,
    CollectionPreviewed(ScopedResult<CollectionSelectionPreview>),
    CreateCollection,
    CollectionCreated(ScopedResult<DashboardSnapshot>),
    UpdateCollection(CollectionId),
    CollectionUpdated(ScopedResult<DashboardSnapshot>),
    EditCollection(CollectionId),
    CancelCollectionEdit,
    EditCollectionNameChanged(String),
    EditSelectionModeChanged(SelectionMode),
    EditSelectionChanged(String),
    EditCategoryDepthChanged(String),
    EditHistoryModeChanged(HistoryMode),
    EditHistoryValueChanged(String),
    EditMaximumPagesChanged(String),
    EditMaximumBytesChanged(String),
    EditRemovalPolicyChanged(CollectionRemovalPolicy),
    EditImageModeChanged(ImageMode),
    EditThumbnailEdgeChanged(String),
    EditThumbnailCountChanged(String),
    EditThumbnailBytesChanged(String),
    EditCollectionScheduleModeChanged(ScheduleMode),
    EditCollectionScheduleValueChanged(String),
    EditCollectionScheduleJitterChanged(String),
    EditCollectionSchedulePaused(bool),
    PreviewCollectionEdit,
    CollectionEditPreviewed(ScopedResult<CollectionSelectionPreview>),
    SaveCollectionEdit,
    CollectionEditSaved(ScopedResult<DashboardSnapshot>),
    PreviewRemoveCollection(CollectionId),
    CancelRemoveCollection,
    ConfirmRemoveCollection,
    CollectionRemoved(ScopedResult<DashboardSnapshot>),
    OpenCollectionPurge(CollectionId),
    RefreshCollectionPurgePreview,
    CollectionPurgePreviewed(ScopedResult<PurgePreview>),
    CollectionPurgeNameChanged(String),
    CollectionPurgeFingerprintChanged(String),
    CollectionPurgePayloadAcknowledged(bool),
    CollectionPurgeExternalCopiesAcknowledged(bool),
    CancelCollectionPurge,
    ConfirmCollectionPurge,
    CollectionPurged(ScopedResult<CollectionPurgeExecution>),
    EditSchedule(CollectionId),
    EditScheduleModeChanged(ScheduleMode),
    EditScheduleValueChanged(String),
    EditScheduleJitterChanged(String),
    EditSchedulePaused(bool),
    SaveSchedule,
    ScheduleSaved(ScopedResult<DashboardSnapshot>),
    NetworkConcurrencyChanged(String),
    NetworkRateChanged(String),
    AvoidMeteredNetworksChanged(bool),
    SaveNetworkPolicy,
    NetworkPolicySaved(ScopedResult<DashboardSnapshot>),
    DumpCollectionChanged(String),
    DumpIndexUrlChanged(String),
    DumpIndexDigestChanged(String),
    DumpExpectedDatabaseChanged(String),
    DumpMaxIndexBytesChanged(String),
    DumpMaxArtifactBytesChanged(String),
    DumpMaxTotalArtifactBytesChanged(String),
    DumpMaxArtifactsChanged(String),
    DumpMaxElapsedSecondsChanged(String),
    DumpMaxCompressedBytesChanged(String),
    DumpMaxDecompressedBytesChanged(String),
    DumpMaxPagesChanged(String),
    DumpMaxPageXmlBytesChanged(String),
    DumpMaxTextBytesChanged(String),
    PreviewDumpBootstrap,
    DumpBootstrapPreviewed(ScopedResult<DumpBootstrapPreview>),
    StartDumpBootstrap,
    DumpBootstrapFinished(ScopedResult<DashboardSnapshot>),
    SigningKeyPathChanged(String),
    TrustedHeadPathChanged(String),
    GenerateSigningKey,
    SigningKeyGenerated(ScopedResult<String>),
    ValidateSigningKey,
    SigningKeyValidated(ScopedResult<String>),
    RefreshTrustedHead,
    TrustedHeadRefreshed(ScopedResult<AnchorRefreshResult>),
    VerifyFull,
    VerifyTrustedHead,
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
    network_policy: NetworkTransferPolicy,
    wikis: Vec<StoredWiki>,
    collections: Vec<StoredCollection>,
    tombstoned_collections: Vec<StoredCollection>,
    collection_configurations: Vec<StoredCollectionConfiguration>,
    schedules: Vec<CollectionSchedule>,
    runs: Vec<SyncRunStatus>,
    dump_imports: Vec<DumpImportStatus>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct NetworkPolicyEditor {
    max_concurrent_requests: String,
    max_download_bytes_per_second: String,
    avoid_metered_networks: bool,
}

impl Default for NetworkPolicyEditor {
    fn default() -> Self {
        Self::from_policy(NetworkTransferPolicy::default())
    }
}

impl NetworkPolicyEditor {
    fn from_policy(policy: NetworkTransferPolicy) -> Self {
        Self {
            max_concurrent_requests: policy.max_concurrent_requests().to_string(),
            max_download_bytes_per_second: policy
                .max_download_bytes_per_second()
                .map_or_else(String::new, |value| value.to_string()),
            avoid_metered_networks: policy.avoid_metered_networks(),
        }
    }

    fn policy(&self) -> Result<NetworkTransferPolicy, String> {
        let max_concurrent_requests = self
            .max_concurrent_requests
            .trim()
            .parse::<u32>()
            .map_err(|_| "Maximum concurrent requests must be a positive integer.".to_owned())?;
        let max_download_bytes_per_second = if self.max_download_bytes_per_second.trim().is_empty()
        {
            None
        } else {
            Some(
                self.max_download_bytes_per_second
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| {
                        "Maximum downloaded bytes/second must be a positive integer or blank."
                            .to_owned()
                    })?,
            )
        };
        NetworkTransferPolicy::new(
            max_concurrent_requests,
            max_download_bytes_per_second,
            self.avoid_metered_networks,
        )
        .map_err(|error| error.to_string())
    }
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
    removal_policy: CollectionRemovalPolicy,
    image_mode: ImageMode,
    thumbnail_max_edge_pixels: String,
    thumbnail_max_images_per_revision: String,
    thumbnail_max_bytes_per_image: String,
    schedule_mode: ScheduleMode,
    schedule_value: String,
    schedule_jitter_minutes: String,
    schedule_paused: bool,
}

impl Default for CollectionForm {
    fn default() -> Self {
        let thumbnail_policy = ThumbnailPolicy::default();
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
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_mode: ImageMode::None,
            thumbnail_max_edge_pixels: thumbnail_policy.maximum_edge_pixels().get().to_string(),
            thumbnail_max_images_per_revision: thumbnail_policy
                .maximum_images_per_revision()
                .get()
                .to_string(),
            thumbnail_max_bytes_per_image: thumbnail_policy
                .maximum_bytes_per_image()
                .get()
                .to_string(),
            schedule_mode: ScheduleMode::Manual,
            schedule_value: String::new(),
            schedule_jitter_minutes: "0".to_owned(),
            schedule_paused: false,
        }
    }
}

#[derive(Clone, Debug)]
struct CollectionEditor {
    collection_id: CollectionId,
    expected_generation: u64,
    form: CollectionForm,
    preview: Option<CollectionSelectionPreview>,
}

impl CollectionForm {
    fn from_configuration(
        configuration: &StoredCollectionConfiguration,
        wiki: &StoredWiki,
        schedule: Option<CollectionSchedule>,
    ) -> Self {
        let (selection_mode, selection, category_depth) = match &configuration.rule {
            CollectionRule::ExplicitTitles(titles) => (
                SelectionMode::Titles,
                titles
                    .iter()
                    .map(PageTitle::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                "0".to_owned(),
            ),
            CollectionRule::TitleList(titles) => (
                SelectionMode::TitleList,
                titles
                    .iter()
                    .map(PageTitle::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                "0".to_owned(),
            ),
            CollectionRule::Category {
                title,
                recursion_depth,
            } => (
                SelectionMode::Category,
                title.as_str().to_owned(),
                recursion_depth.to_string(),
            ),
        };
        let (history_mode, history_value) = match configuration.history_policy {
            HistoryPolicy::CurrentAndFuture => (HistoryMode::CurrentAndFuture, String::new()),
            HistoryPolicy::LastN(count) => (HistoryMode::LastN, count.get().to_string()),
            HistoryPolicy::Since(timestamp) => {
                (HistoryMode::Since, timestamp.as_seconds().to_string())
            }
            HistoryPolicy::Complete => (HistoryMode::Complete, String::new()),
        };
        let schedule = schedule.unwrap_or(CollectionSchedule {
            collection_id: configuration.collection_id,
            cadence: ScheduleCadence::Manual,
            jitter_seconds: 0,
            paused: false,
            next_run_at: None,
            last_started_at: None,
        });
        let schedule_editor = ScheduleEditor::from_schedule(configuration.collection_id, schedule);
        let (image_mode, thumbnail_policy) = match configuration.image_policy {
            ImagePolicy::None => (ImageMode::None, ThumbnailPolicy::default()),
            ImagePolicy::Thumbnails(policy) => (ImageMode::Thumbnails, policy),
        };
        Self {
            name: configuration.name.clone(),
            language_code: wiki.language_code.clone(),
            api_endpoint: wiki.api_endpoint.clone(),
            selection_mode,
            selection,
            category_depth,
            history_mode,
            history_value,
            maximum_pages: configuration
                .budget
                .maximum_pages()
                .map_or_else(String::new, |value| value.get().to_string()),
            maximum_bytes: configuration
                .budget
                .maximum_bytes()
                .map_or_else(String::new, |value| value.get().to_string()),
            removal_policy: configuration.removal_policy,
            image_mode,
            thumbnail_max_edge_pixels: thumbnail_policy.maximum_edge_pixels().get().to_string(),
            thumbnail_max_images_per_revision: thumbnail_policy
                .maximum_images_per_revision()
                .get()
                .to_string(),
            thumbnail_max_bytes_per_image: thumbnail_policy
                .maximum_bytes_per_image()
                .get()
                .to_string(),
            schedule_mode: schedule_editor.mode,
            schedule_value: schedule_editor.value,
            schedule_jitter_minutes: schedule_editor.jitter_minutes,
            schedule_paused: schedule_editor.paused,
        }
    }

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

    fn image_policy(&self) -> Result<ImagePolicy, String> {
        match self.image_mode {
            ImageMode::None => Ok(ImagePolicy::None),
            ImageMode::Thumbnails => {
                let maximum_edge_pixels = self
                    .thumbnail_max_edge_pixels
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "Thumbnail edge must be a positive pixel count.".to_owned())?;
                let maximum_images_per_revision = self
                    .thumbnail_max_images_per_revision
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        "Thumbnail count must be a positive number per revision.".to_owned()
                    })?;
                let maximum_bytes_per_image = self
                    .thumbnail_max_bytes_per_image
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| {
                        "Thumbnail byte limit must be a positive number per image.".to_owned()
                    })?;
                ThumbnailPolicy::new(
                    maximum_edge_pixels,
                    maximum_images_per_revision,
                    maximum_bytes_per_image,
                )
                .map(ImagePolicy::Thumbnails)
                .map_err(|error| error.to_string())
            }
        }
    }

    fn schedule(&self) -> Result<ScheduleSettings, String> {
        parse_schedule_settings(
            self.schedule_mode,
            &self.schedule_value,
            &self.schedule_jitter_minutes,
            self.schedule_paused,
        )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImageMode {
    None,
    Thumbnails,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScheduleMode {
    Manual,
    Interval,
    DailyUtc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScheduleSettings {
    cadence: ScheduleCadence,
    jitter_seconds: u32,
    paused: bool,
}

#[derive(Clone, Debug)]
struct ScheduleEditor {
    collection_id: CollectionId,
    mode: ScheduleMode,
    value: String,
    jitter_minutes: String,
    paused: bool,
}

impl ScheduleEditor {
    fn from_schedule(collection_id: CollectionId, schedule: CollectionSchedule) -> Self {
        let (mode, value) = match schedule.cadence {
            ScheduleCadence::Manual => (ScheduleMode::Manual, String::new()),
            ScheduleCadence::Interval(interval) => (
                ScheduleMode::Interval,
                (interval.seconds() / 60).to_string(),
            ),
            ScheduleCadence::DailyUtc(time) => {
                let seconds = time.seconds_after_midnight();
                (
                    ScheduleMode::DailyUtc,
                    format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60),
                )
            }
        };
        Self {
            collection_id,
            mode,
            value,
            jitter_minutes: (schedule.jitter_seconds / 60).to_string(),
            paused: schedule.paused,
        }
    }

    fn settings(&self) -> Result<ScheduleSettings, String> {
        parse_schedule_settings(self.mode, &self.value, &self.jitter_minutes, self.paused)
    }
}

#[derive(Clone, Debug)]
struct PreviewCollectionRequest {
    api_endpoint: String,
    network_policy: NetworkTransferPolicy,
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
    removal_policy: CollectionRemovalPolicy,
    image_policy: ImagePolicy,
    schedule: ScheduleSettings,
}

#[derive(Clone, Debug)]
struct EditCollectionRequest {
    library_path: PathBuf,
    collection_id: CollectionId,
    expected_generation: u64,
    wiki_id: WikiId,
    name: String,
    preview: CollectionSelectionPreview,
    history_policy: HistoryPolicy,
    budget: CollectionBudget,
    removal_policy: CollectionRemovalPolicy,
    image_policy: ImagePolicy,
    schedule: ScheduleSettings,
}

#[derive(Clone, Debug)]
struct CollectionPurgeDialog {
    collection_id: CollectionId,
    preview: Option<PurgePreview>,
    typed_name: String,
    typed_fingerprint: String,
    payload_only_acknowledged: bool,
    external_copies_acknowledged: bool,
}

impl CollectionPurgeDialog {
    fn new(collection_id: CollectionId) -> Self {
        Self {
            collection_id,
            preview: None,
            typed_name: String::new(),
            typed_fingerprint: String::new(),
            payload_only_acknowledged: false,
            external_copies_acknowledged: false,
        }
    }

    fn install_preview(&mut self, preview: PurgePreview) {
        self.clear_confirmations();
        self.preview = Some(preview);
    }

    fn clear_confirmations(&mut self) {
        self.typed_name.clear();
        self.typed_fingerprint.clear();
        self.payload_only_acknowledged = false;
        self.external_copies_acknowledged = false;
    }

    fn is_confirmed(&self) -> bool {
        self.preview.as_ref().is_some_and(|preview| {
            self.typed_name == preview.collection_name
                && self.typed_fingerprint == preview.fingerprint
                && self.payload_only_acknowledged
                && self.external_copies_acknowledged
        })
    }

    fn confirmed_request(&self) -> Option<CollectionPurgeRequest> {
        let preview = self.preview.as_ref()?;
        self.is_confirmed().then(|| CollectionPurgeRequest {
            collection_id: preview.collection_id,
            collection_name: self.typed_name.clone(),
            preview_fingerprint: self.typed_fingerprint.clone(),
            payload_only_acknowledged: self.payload_only_acknowledged,
            external_copies_not_erased_acknowledged: self.external_copies_acknowledged,
        })
    }
}

#[derive(Clone, Debug)]
struct CollectionPurgeExecution {
    snapshot: DashboardSnapshot,
    outcome: CollectionPurgeOutcome,
}

#[derive(Clone, Debug)]
enum VerificationState {
    NotRun,
    Running(VerificationKind),
    Complete(VerificationReport),
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
enum VerificationKind {
    Local,
    TrustedHead,
    AnchorRefresh,
}

#[derive(Clone, Debug)]
struct AnchorRefreshResult {
    report: VerificationReport,
    summary: String,
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

fn edit_preview_task(key: RequestKey, request: PreviewCollectionRequest) -> Task<Message> {
    Task::perform(
        async move {
            let result = preview_collection(request).await;
            ScopedResult { key, result }
        },
        Message::CollectionEditPreviewed,
    )
}

fn edit_collection_task(key: RequestKey, request: EditCollectionRequest) -> Task<Message> {
    Task::perform(
        async move {
            let result = edit_collection(request).await;
            ScopedResult { key, result }
        },
        Message::CollectionEditSaved,
    )
}

fn remove_collection_task(
    key: RequestKey,
    path: PathBuf,
    collection_id: CollectionId,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = remove_collection(path, collection_id).await;
            ScopedResult { key, result }
        },
        Message::CollectionRemoved,
    )
}

fn purge_preview_task(
    key: RequestKey,
    path: PathBuf,
    collection_id: CollectionId,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = preview_collection_purge(&path, collection_id);
            ScopedResult { key, result }
        },
        Message::CollectionPurgePreviewed,
    )
}

fn purge_collection_task(
    key: RequestKey,
    path: PathBuf,
    request: CollectionPurgeRequest,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = execute_collection_purge(path, request).await;
            ScopedResult { key, result }
        },
        Message::CollectionPurged,
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

fn save_schedule_task(
    key: RequestKey,
    path: PathBuf,
    collection_id: CollectionId,
    schedule: ScheduleSettings,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = save_collection_schedule(path, collection_id, schedule).await;
            ScopedResult { key, result }
        },
        Message::ScheduleSaved,
    )
}

fn save_network_policy_task(
    key: RequestKey,
    path: PathBuf,
    policy: NetworkTransferPolicy,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = save_network_transfer_policy(path, policy).await;
            ScopedResult { key, result }
        },
        Message::NetworkPolicySaved,
    )
}

fn dump_preview_task(key: RequestKey, path: PathBuf, form: DumpBootstrapForm) -> Task<Message> {
    Task::perform(
        async move {
            let result = form.preview(&path);
            ScopedResult { key, result }
        },
        Message::DumpBootstrapPreviewed,
    )
}

fn dump_bootstrap_task(
    key: RequestKey,
    path: PathBuf,
    preview: DumpBootstrapPreview,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = start_dump_bootstrap(path, preview).await;
            ScopedResult { key, result }
        },
        Message::DumpBootstrapFinished,
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

fn trusted_verification_task(key: RequestKey, trusted_head_path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let result = verify_against_trusted_head(key.path.clone(), &trusted_head_path).await;
            ScopedResult { key, result }
        },
        Message::VerificationFinished,
    )
}

fn generate_signing_key_task(key: RequestKey, signing_key_path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let result = generate_signing_key(&signing_key_path);
            ScopedResult { key, result }
        },
        Message::SigningKeyGenerated,
    )
}

fn validate_signing_key_task(key: RequestKey, signing_key_path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let result = load_signing_key(&signing_key_path).map(|_| {
                format!(
                    "Validated the protected Ed25519 signing key at {}.",
                    signing_key_path.display()
                )
            });
            ScopedResult { key, result }
        },
        Message::SigningKeyValidated,
    )
}

fn refresh_trusted_head_task(
    key: RequestKey,
    signing_key_path: PathBuf,
    trusted_head_path: PathBuf,
) -> Task<Message> {
    Task::perform(
        async move {
            let result =
                refresh_trusted_head(key.path.clone(), &signing_key_path, &trusted_head_path).await;
            ScopedResult { key, result }
        },
        Message::TrustedHeadRefreshed,
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
    let _writer_lease = if create {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
        Some(WriterLease::acquire(path).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let library = if create {
        Library::open(path)
    } else {
        Library::open_read_only(path)
    }
    .map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn create_collection(request: CreateCollectionRequest) -> Result<DashboardSnapshot, String> {
    create_collection_and_sync(&request).await
}

async fn preview_collection(
    request: PreviewCollectionRequest,
) -> Result<CollectionSelectionPreview, String> {
    enforce_metered_policy(request.network_policy)?;
    let config = configured_client(&request.api_endpoint, request.network_policy)?;
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
    match WriterAccess::discover(&request.library_path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(_lease) => {
            let mut library =
                Library::open(&request.library_path).map_err(|error| error.to_string())?;
            let network_policy = library
                .network_transfer_policy()
                .map_err(|error| error.to_string())?;
            enforce_metered_policy(network_policy)?;
            let client_config = configured_client(&request.api_endpoint, network_policy)?;
            let source_outcome = administer_source_direct(
                &mut library,
                SourceAdministration::Add {
                    api_endpoint: client_config.endpoint().as_str().to_owned(),
                    language_code: request.language_code.clone(),
                },
            )
            .map_err(|error| error.to_string())?;
            let (wiki_id, source_created) = added_source(source_outcome)?;
            let draft = create_draft(request, wiki_id);
            let outcome = administer_collection_direct(
                &mut library,
                CollectionAdministration::AddWithImagePolicy {
                    draft,
                    image_policy: request.image_policy,
                },
            )
            .map_err(|error| collection_add_error(&error, source_created.then_some(wiki_id)))?;
            let collection_id = added_collection_id(outcome)?;
            set_schedule_direct(&mut library, collection_id, request.schedule)?;
            let client = wikisync_mediawiki::MediaWikiClient::new(client_config)
                .map_err(|error| error.to_string())?;
            bootstrap_collection(&client, &mut library, collection_id)
                .await
                .map_err(|error| error.to_string())?;
            snapshot(&library)
        }
        WriterAccess::Daemon(client) => {
            let library = Library::open_read_only(&request.library_path)
                .map_err(|error| error.to_string())?;
            let endpoint = configured_client(
                &request.api_endpoint,
                library
                    .network_transfer_policy()
                    .map_err(|error| error.to_string())?,
            )?
            .endpoint()
            .as_str()
            .to_owned();
            let existing_wiki_id = library
                .wikis()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|wiki| {
                    wiki.api_endpoint == endpoint && wiki.language_code == request.language_code
                })
                .map(|wiki| wiki.wiki_id);
            drop(library);
            let (wiki_id, source_created) = if let Some(wiki_id) = existing_wiki_id {
                (wiki_id, false)
            } else {
                let outcome = client
                    .administer_source(SourceAdministration::Add {
                        api_endpoint: endpoint,
                        language_code: request.language_code.clone(),
                    })
                    .map_err(|error| error.to_string())?;
                added_source(outcome)?
            };
            let outcome = client
                .administer_collection(CollectionAdministration::AddWithImagePolicy {
                    draft: create_draft(request, wiki_id),
                    image_policy: request.image_policy,
                })
                .map_err(|error| collection_add_error(&error, source_created.then_some(wiki_id)))?;
            let collection_id = added_collection_id(outcome)?;
            client
                .forward_mutation(set_collection_schedule_mutation(
                    collection_id.get(),
                    request.schedule.cadence,
                    request.schedule.jitter_seconds,
                    request.schedule.paused,
                ))
                .map_err(|error| {
                    format!(
                        "Collection {collection_id} was created, but its schedule could not be saved: {error}"
                    )
                })?;
            client
                .forward_mutation(Mutation::SyncCollection(collection_id.get()))
                .map_err(|error| {
                    format!(
                        "Collection {collection_id} was created and scheduled, but its first synchronization did not complete: {error}"
                    )
                })?;
            let library = Library::open_read_only(&request.library_path)
                .map_err(|error| error.to_string())?;
            snapshot(&library)
        }
    }
}

fn added_source(outcome: SourceAdministrationOutcome) -> Result<(WikiId, bool), String> {
    match outcome {
        SourceAdministrationOutcome::Added {
            wiki_id, created, ..
        } => Ok((wiki_id, created)),
        SourceAdministrationOutcome::Removed { .. } => {
            Err("Source administration returned an unexpected result.".to_owned())
        }
    }
}

fn collection_add_error(
    error: &impl std::fmt::Display,
    newly_registered_source: Option<WikiId>,
) -> String {
    let detail = error.to_string();
    match newly_registered_source {
        Some(wiki_id) => format!(
            "Source wiki {wiki_id} was registered successfully, but collection creation failed: {detail} The source remains configured and can be reused."
        ),
        None => detail,
    }
}

fn create_draft(request: &CreateCollectionRequest, wiki_id: WikiId) -> CollectionDraft {
    CollectionDraft {
        wiki_id,
        name: request.name.clone(),
        preview: request.preview.clone(),
        history_policy: request.history_policy,
        budget: request.budget,
        removal_policy: request.removal_policy,
    }
}

fn added_collection_id(outcome: CollectionAdministrationOutcome) -> Result<CollectionId, String> {
    match outcome {
        CollectionAdministrationOutcome::Added { collection_id, .. } => Ok(collection_id),
        _ => Err("Collection administration returned an unexpected result.".to_owned()),
    }
}

fn set_schedule_direct(
    library: &mut Library,
    collection_id: CollectionId,
    schedule: ScheduleSettings,
) -> Result<(), String> {
    let next_run_at = next_occurrence_after(
        schedule.cadence,
        collection_id.get(),
        schedule.jitter_seconds,
        unix_time_seconds()?,
    );
    library
        .set_collection_schedule(
            collection_id,
            schedule.cadence,
            schedule.jitter_seconds,
            schedule.paused,
            next_run_at,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn edit_collection(request: EditCollectionRequest) -> Result<DashboardSnapshot, String> {
    if request.name.is_empty() {
        return Err("Collection name is required.".to_owned());
    }
    let draft = CollectionDraft {
        wiki_id: request.wiki_id,
        name: request.name,
        preview: request.preview,
        history_policy: request.history_policy,
        budget: request.budget,
        removal_policy: request.removal_policy,
    };
    match WriterAccess::discover(&request.library_path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(_lease) => {
            let mut library =
                Library::open(&request.library_path).map_err(|error| error.to_string())?;
            let outcome = administer_collection_direct(
                &mut library,
                CollectionAdministration::EditWithImagePolicy {
                    collection_id: request.collection_id,
                    expected_generation: request.expected_generation,
                    draft,
                    image_policy: request.image_policy,
                },
            )
            .map_err(|error| collection_edit_error(&error))?;
            ensure_edited_outcome(outcome, request.collection_id)?;
            set_schedule_direct(&mut library, request.collection_id, request.schedule)?;
            snapshot(&library)
        }
        WriterAccess::Daemon(client) => {
            let outcome = client
                .administer_collection(CollectionAdministration::EditWithImagePolicy {
                    collection_id: request.collection_id,
                    expected_generation: request.expected_generation,
                    draft,
                    image_policy: request.image_policy,
                })
                .map_err(|error| collection_edit_error(&error))?;
            ensure_edited_outcome(outcome, request.collection_id)?;
            client
                .forward_mutation(set_collection_schedule_mutation(
                    request.collection_id.get(),
                    request.schedule.cadence,
                    request.schedule.jitter_seconds,
                    request.schedule.paused,
                ))
                .map_err(|error| {
                    format!(
                        "Collection {} was edited, but its schedule could not be saved: {error}",
                        request.collection_id
                    )
                })?;
            let library = Library::open_read_only(&request.library_path)
                .map_err(|error| error.to_string())?;
            snapshot(&library)
        }
    }
}

fn collection_edit_error(error: &impl std::fmt::Display) -> String {
    let detail = error.to_string();
    if detail.contains("changed while it was being previewed")
        || detail.contains("stale collection generation")
    {
        format!(
            "The collection changed after this edit was loaded, so the stale preview was not applied. Reload the collection, preview it again, and then save. Details: {detail}"
        )
    } else {
        detail
    }
}

fn ensure_edited_outcome(
    outcome: CollectionAdministrationOutcome,
    expected: CollectionId,
) -> Result<(), String> {
    match outcome {
        CollectionAdministrationOutcome::Edited { collection_id, .. }
            if collection_id == expected =>
        {
            Ok(())
        }
        _ => Err("Collection administration returned an unexpected result.".to_owned()),
    }
}

async fn remove_collection(
    path: PathBuf,
    collection_id: CollectionId,
) -> Result<DashboardSnapshot, String> {
    let administration = CollectionAdministration::Remove { collection_id };
    match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(&path).map_err(|error| error.to_string())?;
            let outcome = administer_collection_direct(&mut library, administration)
                .map_err(|error| error.to_string())?;
            ensure_removed_outcome(outcome, collection_id)?;
            snapshot(&library)
        }
        WriterAccess::Daemon(client) => {
            let outcome = client
                .administer_collection(administration)
                .map_err(|error| error.to_string())?;
            ensure_removed_outcome(outcome, collection_id)?;
            let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
            snapshot(&library)
        }
    }
}

fn preview_collection_purge(
    path: &Path,
    collection_id: CollectionId,
) -> Result<PurgePreview, String> {
    let library = Library::open_read_only(path).map_err(|error| error.to_string())?;
    library
        .preview_collection_purge(collection_id)
        .map_err(|error| error.to_string())
}

async fn execute_collection_purge(
    path: PathBuf,
    request: CollectionPurgeRequest,
) -> Result<CollectionPurgeExecution, String> {
    let mutation = collection_purge_mutation(&request).map_err(|error| error.to_string())?;
    match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(writer_lease) => {
            // Keep the cooperative lease alive until the terminal receipt has been
            // decoded and the post-purge read-only snapshot has been collected.
            let mut handler = ApplicationHandler::new(&path).map_err(|error| error.to_string())?;
            let wire_outcome = handler
                .mutate(mutation, OperationControl::running())
                .map_err(|error| error.to_string())?;
            let outcome = decode_collection_purge_outcome(&wire_outcome)
                .map_err(|error| error.to_string())?;
            ensure_completed_purge_outcome(&outcome)?;
            let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
            let snapshot = snapshot(&library)?;
            drop(writer_lease);
            Ok(CollectionPurgeExecution { snapshot, outcome })
        }
        WriterAccess::Daemon(client) => {
            let wire_outcome = client
                .forward_mutation(mutation)
                .map_err(|error| error.to_string())?;
            let outcome = decode_collection_purge_outcome(&wire_outcome)
                .map_err(|error| error.to_string())?;
            ensure_completed_purge_outcome(&outcome)?;
            let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
            let snapshot = snapshot(&library)?;
            Ok(CollectionPurgeExecution { snapshot, outcome })
        }
    }
}

fn ensure_completed_purge_outcome(outcome: &CollectionPurgeOutcome) -> Result<(), String> {
    if outcome.progress.state == PurgeJournalState::Succeeded {
        Ok(())
    } else {
        Err(format!(
            "Purge {} returned without a durable succeeded state ({:?}).",
            outcome.purge_id, outcome.progress.state
        ))
    }
}

fn purge_outcome_summary(outcome: &CollectionPurgeOutcome) -> String {
    let progress = &outcome.progress;
    format!(
        "Purge {} completed durably: {} files / {} retired, {} replacement bytes written, {} net bytes reclaimed, and {} packs retired. Audit metadata and hashes remain.",
        outcome.purge_id,
        progress.retired_file_count,
        format_bytes(progress.retired_file_bytes),
        format_bytes(progress.replacement_file_bytes),
        format_signed_bytes(progress.net_reclaimed_file_bytes),
        progress.retired_pack_count,
    )
}

fn format_signed_bytes(bytes: i64) -> String {
    if bytes < 0 {
        format!("-{}", format_bytes(bytes.unsigned_abs()))
    } else {
        format_bytes(bytes as u64)
    }
}

fn ensure_removed_outcome(
    outcome: CollectionAdministrationOutcome,
    expected: CollectionId,
) -> Result<(), String> {
    match outcome {
        CollectionAdministrationOutcome::Removed { collection_id } if collection_id == expected => {
            Ok(())
        }
        _ => Err("Collection administration returned an unexpected result.".to_owned()),
    }
}

async fn update_collection(
    path: PathBuf,
    collection_id: CollectionId,
) -> Result<DashboardSnapshot, String> {
    let _writer_lease = match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(lease) => lease,
        WriterAccess::Daemon(client) => {
            client
                .forward_mutation(Mutation::SyncCollection(collection_id.get()))
                .map_err(|error| error.to_string())?;
            let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
            return snapshot(&library);
        }
    };
    let mut library = Library::open(&path).map_err(|error| error.to_string())?;
    let configuration = library
        .collection_configuration(collection_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Collection has no committed configuration.".to_owned())?;
    let wiki = library
        .wiki(configuration.wiki_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Collection source is missing.".to_owned())?;
    let network_policy = library
        .network_transfer_policy()
        .map_err(|error| error.to_string())?;
    enforce_metered_policy(network_policy)?;
    let client_config = configured_client(&wiki.api_endpoint, network_policy)?;
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

async fn start_dump_bootstrap(
    path: PathBuf,
    preview: DumpBootstrapPreview,
) -> Result<DashboardSnapshot, String> {
    let request = preview
        .draft
        .request()?
        .with_expected_collection_generation(preview.collection_generation);
    let current_generation = collection_generation(&path, preview.draft.collection_id)?;
    if current_generation != preview.collection_generation {
        return Err(
            "The collection changed after the dump bootstrap preview. Refresh, preview the current scope and budgets again, then confirm."
                .to_owned(),
        );
    }
    match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(&path).map_err(|error| error.to_string())?;
            bootstrap_collection_from_current_dump_direct_async(&mut library, &request)
                .await
                .map_err(|error| error.to_string())?;
        }
        WriterAccess::Daemon(client) => {
            client
                .bootstrap_collection_from_current_dump(&request)
                .map_err(|error| error.to_string())?;
        }
    }
    let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn save_collection_schedule(
    path: PathBuf,
    collection_id: CollectionId,
    schedule: ScheduleSettings,
) -> Result<DashboardSnapshot, String> {
    match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Daemon(client) => {
            client
                .forward_mutation(set_collection_schedule_mutation(
                    collection_id.get(),
                    schedule.cadence,
                    schedule.jitter_seconds,
                    schedule.paused,
                ))
                .map_err(|error| error.to_string())?;
        }
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(&path).map_err(|error| error.to_string())?;
            let next_run_at = next_occurrence_after(
                schedule.cadence,
                collection_id.get(),
                schedule.jitter_seconds,
                unix_time_seconds()?,
            );
            library
                .set_collection_schedule(
                    collection_id,
                    schedule.cadence,
                    schedule.jitter_seconds,
                    schedule.paused,
                    next_run_at,
                )
                .map_err(|error| error.to_string())?;
        }
    }
    let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
    snapshot(&library)
}

async fn save_network_transfer_policy(
    path: PathBuf,
    policy: NetworkTransferPolicy,
) -> Result<DashboardSnapshot, String> {
    match WriterAccess::discover(&path).map_err(|error| error.to_string())? {
        WriterAccess::Daemon(client) => {
            client
                .forward_mutation(set_network_transfer_policy_mutation(policy))
                .map_err(|error| error.to_string())?;
        }
        WriterAccess::Direct(_lease) => {
            let mut library = Library::open(&path).map_err(|error| error.to_string())?;
            library
                .update_network_transfer_policy(policy)
                .map_err(|error| error.to_string())?;
        }
    }
    let library = Library::open_read_only(&path).map_err(|error| error.to_string())?;
    snapshot(&library)
}

fn configured_client(
    api_endpoint: &str,
    policy: NetworkTransferPolicy,
) -> Result<ClientConfig, String> {
    let max_concurrent_requests = usize::try_from(policy.max_concurrent_requests())
        .map_err(|_| "Maximum concurrent request policy is too large.".to_owned())?;
    let max_downloaded_response_bytes_per_second = policy
        .max_download_bytes_per_second()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| "Maximum byte-rate policy is too large.".to_owned())?;
    ClientConfig::new(
        api_endpoint,
        application_user_agent().map_err(|error| error.to_string())?,
    )
    .and_then(|config| config.with_max_concurrent_requests(max_concurrent_requests))
    .and_then(|config| {
        config
            .with_max_downloaded_response_bytes_per_second(max_downloaded_response_bytes_per_second)
    })
    .map_err(|error| error.to_string())
}

fn enforce_metered_policy(policy: NetworkTransferPolicy) -> Result<(), String> {
    if !policy.avoid_metered_networks() {
        return Ok(());
    }
    let status = detect_metered_network();
    if status.state == MeteredNetworkState::Metered {
        return Err(
            "Synchronization is blocked by the library policy while the active network is metered."
                .to_owned(),
        );
    }
    Ok(())
}

fn unix_time_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is before the Unix epoch.".to_owned())
}

async fn verify_all_objects(path: PathBuf) -> Result<VerificationReport, String> {
    let library = Library::open_read_only(path).map_err(|error| error.to_string())?;
    verify_library(&library, VerificationScope::Full).map_err(|error| error.to_string())
}

async fn verify_against_trusted_head(
    library_path: PathBuf,
    trusted_head_path: &Path,
) -> Result<VerificationReport, String> {
    let trusted_head = load_trusted_head(trusted_head_path)?;
    let library = Library::open_read_only(library_path).map_err(|error| error.to_string())?;
    verify_library_against_trusted_head(
        &library,
        VerificationOptions::new(VerificationScope::Full),
        &trusted_head,
    )
    .map_err(|error| error.to_string())
}

async fn refresh_trusted_head(
    library_path: PathBuf,
    signing_key_path: &Path,
    trusted_head_path: &Path,
) -> Result<AnchorRefreshResult, String> {
    let signing_key = load_signing_key(signing_key_path)?;
    let library = Library::open_read_only(library_path).map_err(|error| error.to_string())?;
    let trusted_head = sign_current_manifest_head(&library, &signing_key)
        .map_err(|error| format!("Could not sign the current manifest head: {error}"))?;
    let report = verify_library_against_trusted_head(
        &library,
        VerificationOptions::new(VerificationScope::Full),
        &trusted_head,
    )
    .map_err(|error| format!("Pre-publication full verification failed: {error}"))?;
    if !report.is_authenticated_against_trusted_head() {
        return Err(format!(
            "Refusing to publish the trusted head because full authenticated verification retained {} finding(s) or incomplete coverage. The library may have changed during verification; investigate it without replacing the external anchor.",
            report.finding_count
        ));
    }
    let canonical = trusted_head
        .to_canonical_json()
        .map_err(|error| error.to_string())?;
    let retained = write_trusted_head(trusted_head_path, &canonical)?;
    let retained = retained.map_or_else(String::new, |path| {
        format!(" The previous anchor was retained at {}.", path.display())
    });
    Ok(AnchorRefreshResult {
        report,
        summary: format!(
            "Full verification passed and trusted manifest head {} was signed to {} (public-key ID {}).{} Keep the anchor separate from every library copy.",
            trusted_head.sequence,
            trusted_head_path.display(),
            public_key_id(trusted_head.public_key()),
            retained,
        ),
    })
}

fn explicit_artifact_path(
    library_path: &str,
    artifact_path: &str,
    label: &str,
) -> Result<PathBuf, String> {
    let artifact = PathBuf::from(artifact_path.trim());
    if artifact.as_os_str().is_empty() {
        return Err(format!("{label} path is required."));
    }
    if !artifact.is_absolute() {
        return Err(format!(
            "{label} path must be absolute so its external location is unambiguous."
        ));
    }
    let file_name = artifact
        .file_name()
        .ok_or_else(|| format!("{label} path must name a file."))?;
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("{label} path must have a parent directory."))?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        format!(
            "Cannot access the parent directory for {label} at {}: {error}",
            parent.display()
        )
    })?;
    let resolved = canonical_parent.join(file_name);
    let library = Path::new(library_path.trim())
        .canonicalize()
        .map_err(|error| format!("Cannot resolve the library directory: {error}"))?;
    if resolved.starts_with(&library) {
        return Err(format!(
            "{label} must be stored outside the library directory; choose a separately retained location."
        ));
    }
    match fs::symlink_metadata(&resolved) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("{label} path must not be a symbolic link."))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(format!("{label} path must name a regular file."))
        }
        Ok(_) => Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        Err(error) => Err(format!("Cannot inspect {label} path: {error}")),
    }
}

fn generate_signing_key(path: &Path) -> Result<String, String> {
    let signing_key = ManifestSigningKey::generate().map_err(|error| error.to_string())?;
    let bytes = signing_key.to_pkcs8_bytes();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        format!(
            "Could not create signing key {} without overwriting an existing file: {error}",
            path.display()
        )
    })?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!("Could not durably write the signing key: {error}"));
    }
    Ok(format!(
        "Generated a protected Ed25519 signing key at {}. Back it up separately; WikiSyncer will not copy it into the library.",
        path.display()
    ))
}

fn load_signing_key(path: &Path) -> Result<ManifestSigningKey, String> {
    ensure_regular_file(path, "Signing key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map_err(|error| format!("Cannot inspect signing-key permissions: {error}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "Signing key {} is accessible to group or other users; change its permissions to 0600 before use.",
                path.display()
            ));
        }
    }
    let bytes = read_bounded_file(path, MAX_SIGNING_KEY_BYTES, "Signing key")?;
    ManifestSigningKey::from_pkcs8(&bytes).map_err(|error| error.to_string())
}

fn load_trusted_head(path: &Path) -> Result<TrustedManifestHead, String> {
    ensure_regular_file(path, "Trusted-head anchor")?;
    let bytes = read_bounded_file(path, MAX_TRUSTED_HEAD_BYTES as u64, "Trusted-head anchor")?;
    TrustedManifestHead::from_canonical_json(&bytes).map_err(|error| error.to_string())
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{label} path must not be a symbolic link."));
    }
    if !metadata.is_file() {
        return Err(format!("{label} path must name a regular file."));
    }
    Ok(())
}

fn read_bounded_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>, String> {
    let length = fs::metadata(path)
        .map_err(|error| format!("Cannot inspect {label}: {error}"))?
        .len();
    if length > maximum {
        return Err(format!("{label} exceeds its {maximum}-byte input limit."));
    }
    fs::read(path).map_err(|error| format!("Cannot read {label} {}: {error}", path.display()))
}

fn write_trusted_head(path: &Path, canonical: &[u8]) -> Result<Option<PathBuf>, String> {
    let retained = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(
                    "Trusted-head destination must be a regular file, not a symlink or directory."
                        .to_owned(),
                );
            }
            let previous = load_trusted_head(path)?;
            let previous_bytes = fs::read(path)
                .map_err(|error| format!("Cannot retain the previous trusted head: {error}"))?;
            if previous_bytes == canonical {
                None
            } else {
                let backup = previous_anchor_path(path, previous.sequence, previous.public_key())?;
                retain_previous_anchor(&backup, &previous_bytes)?;
                Some(backup)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Cannot inspect trusted-head destination: {error}")),
    };

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Trusted-head destination must have a UTF-8 file name.".to_owned())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is before the Unix epoch.".to_owned())?
        .as_nanos();
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    write_new_private_file(&temporary, canonical, "temporary trusted head")?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not atomically install the trusted head: {error}"
        ));
    }
    Ok(retained)
}

fn previous_anchor_path(path: &Path, sequence: u64, public_key: &[u8]) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Trusted-head destination must have a UTF-8 file name.".to_owned())?;
    Ok(path.with_file_name(format!(
        "{file_name}.sequence-{sequence}.key-{}.previous",
        public_key_id(public_key)
    )))
}

fn retain_previous_anchor(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => Err(format!(
            "Refusing to overwrite a different retained anchor at {}.",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_new_private_file(path, bytes, "previous trusted head")
        }
        Err(error) => Err(format!("Cannot inspect retained anchor: {error}")),
    }
}

fn write_new_private_file(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not create {label} {}: {error}", path.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!("Could not durably write {label}: {error}"));
    }
    Ok(())
}

fn public_key_id(public_key: &[u8]) -> String {
    public_key
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    let network_policy = library
        .network_transfer_policy()
        .map_err(|error| error.to_string())?;
    let collections = library.collections().map_err(|error| error.to_string())?;
    let tombstoned_collections = library
        .collections_including_tombstones()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|collection| collection.tombstoned_at.is_some())
        .collect();
    let wikis = library.wikis().map_err(|error| error.to_string())?;
    let collection_configurations = collections
        .iter()
        .map(|collection| {
            library
                .collection_configuration(collection.collection_id)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<Option<StoredCollectionConfiguration>>, String>>()?
        .into_iter()
        .flatten()
        .collect();
    let schedules = library.schedules().map_err(|error| error.to_string())?;
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
    let dump_imports = runs
        .iter()
        .map(|run| {
            library
                .dump_import_status(run.run_id)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<Option<DumpImportStatus>>, String>>()?
        .into_iter()
        .flatten()
        .collect();
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
        network_policy,
        wikis,
        collections,
        tombstoned_collections,
        collection_configurations,
        schedules,
        runs,
        dump_imports,
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

fn collection_generation(path: &Path, collection_id: CollectionId) -> Result<u64, String> {
    let library = Library::open_read_only(path).map_err(|error| error.to_string())?;
    library
        .collection_configuration(collection_id)
        .map_err(|error| error.to_string())?
        .map(|configuration| configuration.generation)
        .ok_or_else(|| "Collection configuration is unavailable; refresh the library.".to_owned())
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

fn mode_button_with(
    label: &str,
    mode: SelectionMode,
    selected: SelectionMode,
    message: fn(SelectionMode) -> Message,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(message(mode)).into()
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

fn history_button_with(
    label: &str,
    mode: HistoryMode,
    selected: HistoryMode,
    message: fn(HistoryMode) -> Message,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(message(mode)).into()
}

fn removal_policy_button(
    label: &str,
    policy: CollectionRemovalPolicy,
    selected: CollectionRemovalPolicy,
    message: fn(CollectionRemovalPolicy) -> Message,
) -> Element<'static, Message> {
    let label = if policy == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(message(policy)).into()
}

fn image_policy_button(
    label: &str,
    mode: ImageMode,
    selected: ImageMode,
    message: fn(ImageMode) -> Message,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(message(mode)).into()
}

fn image_policy_form_summary(form: &CollectionForm) -> String {
    match form.image_policy() {
        Ok(policy) => format!("Configured image policy: {}", image_policy_summary(policy)),
        Err(error) => format!("Thumbnail limits are invalid: {error}"),
    }
}

fn image_policy_summary(policy: ImagePolicy) -> String {
    match policy {
        ImagePolicy::None => "off (text capture is independent of media)".to_owned(),
        ImagePolicy::Thumbnails(policy) => format!(
            "saved preview policy only; thumbnail download is not active in this build — edge ≤ {} px, ≤ {} images/revision, ≤ {} per image",
            policy.maximum_edge_pixels(),
            policy.maximum_images_per_revision(),
            format_bytes(policy.maximum_bytes_per_image().get()),
        ),
    }
}

fn schedule_button(
    label: &str,
    mode: ScheduleMode,
    selected: ScheduleMode,
    message: fn(ScheduleMode) -> Message,
) -> Element<'static, Message> {
    let label = if mode == selected {
        format!("• {label}")
    } else {
        label.to_owned()
    };
    button(text(label)).on_press(message(mode)).into()
}

fn schedule_value_hint(mode: ScheduleMode) -> &'static str {
    match mode {
        ScheduleMode::Manual => "No automatic schedule",
        ScheduleMode::Interval => "Interval in minutes (1–527040)",
        ScheduleMode::DailyUtc => "Daily UTC time (HH:MM)",
    }
}

fn parse_schedule_settings(
    mode: ScheduleMode,
    value: &str,
    jitter_minutes: &str,
    paused: bool,
) -> Result<ScheduleSettings, String> {
    let jitter_minutes = jitter_minutes
        .trim()
        .parse::<u32>()
        .map_err(|_| "Schedule jitter must be a non-negative number of minutes.".to_owned())?;
    let jitter_seconds = jitter_minutes
        .checked_mul(60)
        .ok_or_else(|| "Schedule jitter is too large.".to_owned())?;
    let cadence = match mode {
        ScheduleMode::Manual => {
            if jitter_seconds != 0 {
                return Err("Manual schedules cannot have jitter.".to_owned());
            }
            ScheduleCadence::Manual
        }
        ScheduleMode::Interval => {
            let minutes = value.trim().parse::<u32>().map_err(|_| {
                "Interval schedule requires a positive number of minutes.".to_owned()
            })?;
            let seconds = minutes
                .checked_mul(60)
                .ok_or_else(|| "Schedule interval is too large.".to_owned())?;
            ScheduleCadence::interval(seconds).map_err(|error| error.to_string())?
        }
        ScheduleMode::DailyUtc => {
            let (hours, minutes) = value
                .trim()
                .split_once(':')
                .ok_or_else(|| "Daily UTC schedule must use HH:MM.".to_owned())?;
            let hours = hours
                .parse::<u32>()
                .map_err(|_| "Daily UTC hour must be between 00 and 23.".to_owned())?;
            let minutes = minutes
                .parse::<u32>()
                .map_err(|_| "Daily UTC minute must be between 00 and 59.".to_owned())?;
            if hours >= 24 || minutes >= 60 {
                return Err("Daily UTC schedule must use a time from 00:00 to 23:59.".to_owned());
            }
            ScheduleCadence::daily_utc(hours * 3_600 + minutes * 60)
                .map_err(|error| error.to_string())?
        }
    };
    if let ScheduleCadence::Interval(interval) = cadence
        && jitter_seconds > interval.seconds()
    {
        return Err("Schedule jitter cannot exceed the interval.".to_owned());
    }
    if jitter_seconds > 86_400 {
        return Err("Schedule jitter cannot exceed 1,440 minutes.".to_owned());
    }
    Ok(ScheduleSettings {
        cadence,
        jitter_seconds,
        paused,
    })
}

fn metric<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    container(column![text(value).size(25), text(label).size(13)].spacing(3))
        .padding(12)
        .width(Length::Fill)
        .into()
}

fn verification_report_view(report: &VerificationReport) -> Element<'_, Message> {
    let mut content = column![
        text(format!(
            "Verified {} of {} logical objects ({} canonical bytes), {} of {} manifests, and {} metadata references.",
            report.objects_verified,
            report.objects_at_start,
            report.canonical_bytes_verified,
            report.manifests_identity_verified,
            report.manifests_at_start,
            report.metadata_records_examined,
        )),
        text(if report.is_verified_since_capture() {
            "Complete: the stable captured library is internally verified since capture. This is an integrity result, not proof that its content is true."
        } else {
            "Full verification did not establish complete clean coverage. Review the findings below before trusting or refreshing an anchor."
        }),
        text(format!(
            "{} finding(s); {} detailed finding(s) retained; {} omitted.",
            report.finding_count,
            report.findings.len(),
            report.omitted_findings
        )),
    ]
    .spacing(6);

    let anchor_was_compared = report.trusted_head_authenticated
        || report.findings.iter().any(|finding| {
            matches!(
                finding.kind,
                VerificationFindingKind::TrustedHeadSignatureInvalid
                    | VerificationFindingKind::TrustedHeadMismatch
            )
        });
    if anchor_was_compared {
        content = content.push(text(if report.is_authenticated_against_trusted_head() {
            "External anchor authenticated: its Ed25519 signature is valid and its exact manifest head matches this fully verified library."
        } else {
            "External anchor did not authenticate this library. Do not replace or refresh it until the mismatch or invalid signature is understood."
        }));
    }
    for finding in report.findings.iter().take(8) {
        content = content.push(text(format!("• {:?}: {}", finding.kind, finding.message)).size(13));
    }
    content.into()
}

fn collection_edit_view<'a>(editor: &'a CollectionEditor, enabled: bool) -> Element<'a, Message> {
    let form = &editor.form;
    let selection_hint = match form.selection_mode {
        SelectionMode::Titles => "One page title per line",
        SelectionMode::TitleList => "Paste a newline-delimited title list",
        SelectionMode::Category => "Category:Name",
    };
    let preview_summary = editor.preview.as_ref().map_or_else(
        || "A fresh full preview is required; no edit has been committed.".to_owned(),
        |preview| {
            let bytes = preview.predicted_canonical_bytes.map_or_else(
                || "source bytes unknown until capture".to_owned(),
                format_bytes,
            );
            format!(
                "Complete replacement preview: {} pages, {} missing titles, {bytes}.",
                preview.members.len(),
                preview.missing_titles.len()
            )
        },
    );
    container(
        column![
            text(format!("Edit collection {}", editor.collection_id)).size(22),
            text(format!(
                "Source is fixed for this collection: {} ({})",
                form.language_code, form.api_endpoint
            )),
            text_input("Collection name", &form.name)
                .on_input(Message::EditCollectionNameChanged)
                .padding(10),
            row![
                mode_button_with(
                    "Page titles",
                    SelectionMode::Titles,
                    form.selection_mode,
                    Message::EditSelectionModeChanged,
                ),
                mode_button_with(
                    "Title-list import",
                    SelectionMode::TitleList,
                    form.selection_mode,
                    Message::EditSelectionModeChanged,
                ),
                mode_button_with(
                    "Category",
                    SelectionMode::Category,
                    form.selection_mode,
                    Message::EditSelectionModeChanged,
                ),
            ]
            .spacing(8),
            text_input(selection_hint, &form.selection)
                .on_input(Message::EditSelectionChanged)
                .padding(10),
            text_input("Category recursion depth (0–16)", &form.category_depth)
                .on_input(Message::EditCategoryDepthChanged)
                .padding(10),
            text("History retention").size(17),
            row![
                history_button_with(
                    "Current + future",
                    HistoryMode::CurrentAndFuture,
                    form.history_mode,
                    Message::EditHistoryModeChanged,
                ),
                history_button_with(
                    "Last N",
                    HistoryMode::LastN,
                    form.history_mode,
                    Message::EditHistoryModeChanged,
                ),
                history_button_with(
                    "Since",
                    HistoryMode::Since,
                    form.history_mode,
                    Message::EditHistoryModeChanged,
                ),
                history_button_with(
                    "Complete",
                    HistoryMode::Complete,
                    form.history_mode,
                    Message::EditHistoryModeChanged,
                ),
            ]
            .spacing(8),
            text_input("Last-N count or Since Unix timestamp", &form.history_value)
                .on_input(Message::EditHistoryValueChanged)
                .padding(10),
            row![
                text_input("Hard maximum pages (blank = unlimited)", &form.maximum_pages)
                    .on_input(Message::EditMaximumPagesChanged)
                    .padding(10),
                text_input("Hard maximum canonical bytes (blank = unlimited)", &form.maximum_bytes)
                    .on_input(Message::EditMaximumBytesChanged)
                    .padding(10),
            ]
            .spacing(8),
            text("When a dynamic rule no longer selects a page").size(17),
            row![
                removal_policy_button(
                    "Stop tracking; retain captured history",
                    CollectionRemovalPolicy::StopTrackingRetainHistory,
                    form.removal_policy,
                    Message::EditRemovalPolicyChanged,
                ),
                removal_policy_button(
                    "Keep tracking",
                    CollectionRemovalPolicy::KeepTracking,
                    form.removal_policy,
                    Message::EditRemovalPolicyChanged,
                ),
            ]
            .spacing(8),
            text("Referenced image capture").size(17),
            row![
                image_policy_button(
                    "No images",
                    ImageMode::None,
                    form.image_mode,
                    Message::EditImageModeChanged,
                ),
                image_policy_button(
                    "Bounded thumbnails",
                    ImageMode::Thumbnails,
                    form.image_mode,
                    Message::EditImageModeChanged,
                ),
            ]
            .spacing(8),
            row![
                text_input(
                    "Maximum thumbnail edge (pixels)",
                    &form.thumbnail_max_edge_pixels,
                )
                .on_input(Message::EditThumbnailEdgeChanged)
                .padding(10),
                text_input(
                    "Maximum images per revision",
                    &form.thumbnail_max_images_per_revision,
                )
                .on_input(Message::EditThumbnailCountChanged)
                .padding(10),
                text_input(
                    "Maximum bytes per thumbnail",
                    &form.thumbnail_max_bytes_per_image,
                )
                .on_input(Message::EditThumbnailBytesChanged)
                .padding(10),
            ]
            .spacing(8),
            text(image_policy_form_summary(form)),
            text("Automatic synchronization schedule").size(17),
            row![
                schedule_button(
                    "Manual",
                    ScheduleMode::Manual,
                    form.schedule_mode,
                    Message::EditCollectionScheduleModeChanged,
                ),
                schedule_button(
                    "Interval",
                    ScheduleMode::Interval,
                    form.schedule_mode,
                    Message::EditCollectionScheduleModeChanged,
                ),
                schedule_button(
                    "Daily UTC",
                    ScheduleMode::DailyUtc,
                    form.schedule_mode,
                    Message::EditCollectionScheduleModeChanged,
                ),
            ]
            .spacing(8),
            text_input(
                schedule_value_hint(form.schedule_mode),
                &form.schedule_value,
            )
            .on_input(Message::EditCollectionScheduleValueChanged)
            .padding(10),
            text_input(
                "Maximum jitter in minutes",
                &form.schedule_jitter_minutes,
            )
            .on_input(Message::EditCollectionScheduleJitterChanged)
            .padding(10),
            checkbox("Pause automatic synchronization", form.schedule_paused)
                .on_toggle(Message::EditCollectionSchedulePaused),
            text("If scope changes, absent members follow the selected removal policy. Already captured revisions are retained in either mode."),
            text(preview_summary),
            row![
                button("Cancel").on_press(Message::CancelCollectionEdit),
                button("Preview complete edit")
                    .on_press_maybe(enabled.then_some(Message::PreviewCollectionEdit)),
                button("Save previewed edit").on_press_maybe(
                    (enabled && editor.preview.is_some()).then_some(Message::SaveCollectionEdit)
                ),
            ]
            .spacing(8),
        ]
        .spacing(9),
    )
    .padding(12)
    .into()
}

fn collection_row<'a>(
    collection: &'a StoredCollection,
    configuration: Option<&StoredCollectionConfiguration>,
    schedule: Option<&CollectionSchedule>,
    update_enabled: bool,
) -> Element<'a, Message> {
    let schedule = schedule.map_or_else(
        || "Manual".to_owned(),
        |schedule| {
            let cadence = match schedule.cadence {
                ScheduleCadence::Manual => "Manual".to_owned(),
                ScheduleCadence::Interval(interval) => {
                    format!("Every {} min", interval.seconds() / 60)
                }
                ScheduleCadence::DailyUtc(time) => {
                    let seconds = time.seconds_after_midnight();
                    format!(
                        "Daily {:02}:{:02} UTC",
                        seconds / 3_600,
                        (seconds % 3_600) / 60
                    )
                }
            };
            if schedule.paused {
                format!("{cadence} · paused")
            } else {
                cadence
            }
        },
    );
    container(row![
        column![
            text(&collection.name).size(19),
            text(format!(
                "Collection {} · wiki {}",
                collection.collection_id, collection.wiki_id
            ))
            .size(13),
            text(schedule).size(13),
            text(configuration.map_or_else(
                || "Image policy unavailable".to_owned(),
                |configuration| image_policy_summary(configuration.image_policy),
            ))
            .size(13),
        ],
        Space::new(Length::Fill, Length::Shrink),
        text(format!("{} pages", collection.page_count)),
        button("Update").on_press_maybe(
            update_enabled.then_some(Message::UpdateCollection(collection.collection_id))
        ),
        button("Edit").on_press_maybe(
            update_enabled.then_some(Message::EditCollection(collection.collection_id))
        ),
        button("Schedule").on_press_maybe(
            update_enabled.then_some(Message::EditSchedule(collection.collection_id))
        ),
        button("Stop tracking").on_press_maybe(
            update_enabled.then_some(Message::PreviewRemoveCollection(collection.collection_id))
        ),
    ])
    .padding(12)
    .into()
}

fn tombstoned_collection_row(collection: &StoredCollection, enabled: bool) -> Element<'_, Message> {
    container(
        row![
            column![
                text(&collection.name).size(19),
                text(format!(
                    "Collection {} · stopped at {} · audit/history retained",
                    collection.collection_id,
                    collection
                        .tombstoned_at
                        .map_or_else(|| "unknown".to_owned(), |timestamp| timestamp.to_string())
                ))
                .size(13),
            ],
            Space::new(Length::Fill, Length::Shrink),
            button("Preview payload purge").on_press_maybe(
                enabled.then_some(Message::OpenCollectionPurge(collection.collection_id))
            ),
        ]
        .align_y(Alignment::Center),
    )
    .padding(12)
    .into()
}

fn collection_purge_view(dialog: &CollectionPurgeDialog, enabled: bool) -> Element<'_, Message> {
    let Some(preview) = dialog.preview.as_ref() else {
        return container(
            column![
                text(format!(
                    "Read-only payload purge preview · collection {}",
                    dialog.collection_id
                ))
                .size(20),
                text("Computing the exclusive canonical-payload closure locally. This preview never contacts MediaWiki and does not write to the library."),
                row![
                    button("Cancel").on_press_maybe(enabled.then_some(Message::CancelCollectionPurge)),
                    button("Retry read-only preview")
                        .on_press_maybe(enabled.then_some(Message::RefreshCollectionPurgePreview)),
                ]
                .spacing(8),
            ]
            .spacing(9),
        )
        .padding(12)
        .into();
    };

    let confirmations_match = dialog.is_confirmed();
    container(
        column![
            text("Permanent local payload purge").size(23),
            text(format!("Exact collection name: {}", preview.collection_name)),
            text(format!("Exact preview fingerprint: {}", preview.fingerprint)),
            text(format!("Catalog fingerprint: {}", preview.catalog_fingerprint)),
            text(format!(
                "Objects: {} total · {} wikitext · {} media",
                preview.object_count,
                preview.wikitext_object_count,
                preview.media_object_count
            )),
            text(format!(
                "Logical payload: {} · estimated reclaimable: {}",
                format_bytes(preview.logical_bytes),
                format_bytes(preview.reclaimable_bytes)
            )),
            text(format!(
                "Storage layout: {} loose objects · {} affected packs · {} whole packs · {} mixed packs",
                preview.loose_object_count,
                preview.affected_pack_count,
                preview.whole_pack_count,
                preview.mixed_pack_count
            )),
            horizontal_rule(1),
            text("Audit boundary: this removes only exclusive local payload representations. Collection tombstones, logical metadata, object hashes, manifests, integrity evidence, and operation records remain."),
            text("External-copy warning: backups, filesystem snapshots, exports, replicas, caches, and copies on other devices are outside this purge and are not erased."),
            text("Erasure warning: unlinking and pack replacement are not secure physical erasure. Storage media, wear-leveling, journal, and forensic remnants may persist."),
            text("Type the exact collection name:"),
            text_input("Exact collection name", &dialog.typed_name)
                .on_input(Message::CollectionPurgeNameChanged)
                .padding(10),
            text("Type the exact preview fingerprint:"),
            text_input("Exact preview fingerprint", &dialog.typed_fingerprint)
                .on_input(Message::CollectionPurgeFingerprintChanged)
                .padding(10),
            checkbox(
                "I understand this purges payload only; retained audit metadata, hashes, manifests, and integrity evidence remain.",
                dialog.payload_only_acknowledged,
            )
            .on_toggle(Message::CollectionPurgePayloadAcknowledged),
            checkbox(
                "I understand backups, snapshots, exports, other copies, and physical-device remnants are not erased.",
                dialog.external_copies_acknowledged,
            )
            .on_toggle(Message::CollectionPurgeExternalCopiesAcknowledged),
            row![
                button("Cancel").on_press_maybe(enabled.then_some(Message::CancelCollectionPurge)),
                button("Recompute read-only preview").on_press_maybe(
                    enabled.then_some(Message::RefreshCollectionPurgePreview)
                ),
                button("Permanently purge previewed payload").on_press_maybe(
                    (enabled && confirmations_match).then_some(Message::ConfirmCollectionPurge)
                ),
            ]
            .spacing(8),
        ]
        .spacing(9),
    )
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
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as Shared, Mutex};
    use std::thread;
    use wikisync_core::{PageId, PageTitle, RevisionId};
    use wikisync_store::{CurrentRevisionCapture, DumpImportRequest, SyncRunKind};

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
    const NORWEGIAN_TITLE_RESOLUTION: &str = r#"{
      "batchcomplete": true,
      "query": {
        "normalized": [
          { "from": "Rust_programmeringssprak", "to": "Rust programmeringssprak" }
        ],
        "redirects": [
          { "from": "Rust programmeringssprak", "to": "Rust (programmeringssprak)" }
        ],
        "pages": [
          {
            "pageid": 25357340,
            "ns": 0,
            "title": "Rust (programmeringssprak)",
            "revisions": [
              {
                "revid": 1300000001,
                "parentid": 1300000000,
                "timestamp": "2026-08-19T12:34:56Z",
                "size": 40,
                "sha1": "c34wlley9m7sxty0ey9ammqeq3n0cnk"
              }
            ]
          }
        ]
      }
    }"#;
    const NORWEGIAN_UNCHANGED_HEAD: &str = r#"{
      "batchcomplete": true,
      "query": {
        "pages": [
          {
            "pageid": 25357340,
            "ns": 0,
            "title": "Rust (programmeringssprak)",
            "revisions": [
              {
                "revid": 1300000001,
                "parentid": 1300000000,
                "timestamp": "2026-08-19T12:34:56Z",
                "size": 40,
                "sha1": "c34wlley9m7sxty0ey9ammqeq3n0cnk"
              }
            ]
          }
        ]
      }
    }"#;
    const NORWEGIAN_REVISION_CONTENT: &str = r#"{
      "batchcomplete": true,
      "query": {
        "pages": [
          {
            "pageid": 25357340,
            "ns": 0,
            "title": "Rust (programmeringssprak)",
            "revisions": [
              {
                "revid": 1300000001,
                "parentid": 1300000000,
                "timestamp": "2026-08-19T12:34:56Z",
                "user": "Norsk testredaktor",
                "userid": 84,
                "comment": "Forbedre historieseksjonen",
                "minor": false,
                "size": 40,
                "sha1": "c34wlley9m7sxty0ey9ammqeq3n0cnk",
                "slots": {
                  "main": {
                    "contentmodel": "wikitext",
                    "contentformat": "text/x-wiki",
                    "content": "== Rust ==\nEt systemprogrammeringssprak."
                  }
                }
              }
            ]
          }
        ]
      }
    }"#;

    fn purge_preview_fixture(collection_id: CollectionId, suffix: &str) -> PurgePreview {
        PurgePreview {
            collection_id,
            collection_name: format!("Stopped collection {suffix}"),
            collection_generation: 2,
            tombstoned_at: 1_777_000_000,
            manifest_head_sequence: None,
            manifest_head_id: None,
            catalog_fingerprint: format!("catalog-{suffix}"),
            fingerprint: format!("b3:{suffix}"),
            object_count: 3,
            wikitext_object_count: 2,
            media_object_count: 1,
            logical_bytes: 1_024,
            reclaimable_bytes: 768,
            loose_object_count: 1,
            affected_pack_count: 2,
            whole_pack_count: 1,
            mixed_pack_count: 1,
        }
    }

    #[test]
    fn purge_confirmation_requires_both_exact_strings_and_two_acknowledgements() {
        let collection_id = CollectionId::new(7).unwrap();
        let preview = purge_preview_fixture(collection_id, "abcdef");
        let mut dialog = CollectionPurgeDialog::new(collection_id);
        dialog.install_preview(preview.clone());

        dialog.typed_name = preview.collection_name.clone();
        dialog.typed_fingerprint = preview.fingerprint.clone();
        dialog.payload_only_acknowledged = true;
        assert!(!dialog.is_confirmed());
        assert!(dialog.confirmed_request().is_none());

        dialog.external_copies_acknowledged = true;
        assert!(dialog.is_confirmed());
        let request = dialog.confirmed_request().expect("complete confirmation");
        assert_eq!(request.collection_name, preview.collection_name);
        assert_eq!(request.preview_fingerprint, preview.fingerprint);

        dialog.typed_name.push(' ');
        assert!(!dialog.is_confirmed(), "name matching must be exact");
        dialog.typed_name = preview.collection_name;
        dialog.typed_fingerprint.make_ascii_uppercase();
        assert!(!dialog.is_confirmed(), "fingerprint matching must be exact");
    }

    #[test]
    fn installing_changed_purge_preview_clears_every_confirmation() {
        let collection_id = CollectionId::new(8).unwrap();
        let first = purge_preview_fixture(collection_id, "first");
        let mut dialog = CollectionPurgeDialog::new(collection_id);
        dialog.install_preview(first.clone());
        dialog.typed_name = first.collection_name;
        dialog.typed_fingerprint = first.fingerprint;
        dialog.payload_only_acknowledged = true;
        dialog.external_copies_acknowledged = true;
        assert!(dialog.is_confirmed());

        let changed = purge_preview_fixture(collection_id, "changed");
        dialog.install_preview(changed.clone());
        assert_eq!(dialog.preview, Some(changed));
        assert!(dialog.typed_name.is_empty());
        assert!(dialog.typed_fingerprint.is_empty());
        assert!(!dialog.payload_only_acknowledged);
        assert!(!dialog.external_copies_acknowledged);
        assert!(!dialog.is_confirmed());
    }

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

    #[derive(Debug)]
    struct DumpFixtureServer {
        api_endpoint: String,
        index_url: String,
        index_digest: String,
        requests: Shared<AtomicUsize>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl DumpFixtureServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("dump fixture listener");
            let address = listener.local_addr().expect("dump fixture address");
            let api_endpoint = format!("http://{address}/w/api.php");
            let index_url = format!("http://{address}/index.json");
            let xml = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo><sitename>Fixture</sitename><dbname>enwiki</dbname>
    <base>{api_endpoint}</base><generator>MediaWiki fixture</generator><case>first-letter</case>
    <namespaces><namespace key="0" case="first-letter" /></namespaces>
  </siteinfo>
  <page><title>Alpha</title><ns>0</ns><id>10</id><revision>
    <id>100</id><parentid>99</parentid><timestamp>2026-08-23T10:00:00Z</timestamp>
    <contributor><username>Fixture editor</username><id>42</id></contributor>
    <comment>dump head</comment><model>wikitext</model><format>text/x-wiki</format>
    <text bytes="5" xml:space="preserve">Alpha</text>
  </revision></page>
</mediawiki>"#
            );
            let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
            encoder
                .write_all(xml.as_bytes())
                .expect("compress dump XML");
            let artifact = encoder.finish().expect("finish dump artifact");
            let index = format!(
                r#"{{"schema":"wikisync-current-dump-index-v1","database":"enwiki","generated_at":"2026-08-23T10:02:00Z","artifacts":[{{"kind":"pages-meta-current-multistream","path":"fixture.xml.bz2","bytes":{},"blake3":"{}"}}]}}"#,
                artifact.len(),
                blake3::hash(&artifact).to_hex()
            )
            .into_bytes();
            let index_digest = blake3::hash(&index).to_hex().to_string();
            let unchanged = br#"{
              "batchcomplete":true,"query":{"pages":[{
                "pageid":10,"ns":0,"title":"Alpha","revisions":[{
                  "revid":100,"parentid":99,"timestamp":"2026-08-23T10:00:00Z","size":5
                }]
              }]}}
            "#
            .to_vec();
            let responses = [
                (index, "application/json"),
                (artifact, "application/x-bzip2"),
                (unchanged, "application/json"),
            ];
            let requests = Shared::new(AtomicUsize::new(0));
            let observed = Shared::clone(&requests);
            let thread = thread::spawn(move || {
                for (body, content_type) in responses {
                    let (mut stream, _) = listener.accept().expect("accept dump request");
                    read_fixture_request(&mut stream);
                    observed.fetch_add(1, Ordering::Release);
                    write_fixture_response(&mut stream, &body, content_type);
                }
            });
            Self {
                api_endpoint,
                index_url,
                index_digest,
                requests,
                thread: Some(thread),
            }
        }

        fn finish(mut self) {
            self.thread
                .take()
                .expect("dump fixture thread")
                .join()
                .expect("dump fixture did not panic");
            assert_eq!(self.requests.load(Ordering::Acquire), 3);
        }
    }

    fn read_fixture_request(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("read dump request");
            assert!(read > 0, "client closed before request headers");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() < 64 * 1_024, "dump request too large");
        }
    }

    fn write_fixture_response(stream: &mut TcpStream, body: &[u8], content_type: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write dump response headers");
        stream.write_all(body).expect("write dump response body");
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
        assert_eq!(created.schema_version, 15);
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
        assert_eq!(form.image_policy(), Ok(ImagePolicy::None));

        let thumbnails = CollectionForm {
            image_mode: ImageMode::Thumbnails,
            thumbnail_max_edge_pixels: "640".to_owned(),
            thumbnail_max_images_per_revision: "8".to_owned(),
            thumbnail_max_bytes_per_image: "1048576".to_owned(),
            ..CollectionForm::default()
        };
        let ImagePolicy::Thumbnails(policy) = thumbnails.image_policy().expect("image policy")
        else {
            panic!("expected thumbnails");
        };
        assert_eq!(policy.maximum_edge_pixels().get(), 640);
        assert_eq!(policy.maximum_images_per_revision().get(), 8);
        assert_eq!(policy.maximum_bytes_per_image().get(), 1_048_576);

        let invalid = CollectionForm {
            image_mode: ImageMode::Thumbnails,
            thumbnail_max_edge_pixels: "0".to_owned(),
            ..CollectionForm::default()
        };
        assert!(invalid.image_policy().is_err());
    }

    #[test]
    fn schedule_controls_parse_interval_daily_jitter_and_pause() {
        let interval = parse_schedule_settings(ScheduleMode::Interval, "90", "10", true)
            .expect("interval schedule");
        assert!(matches!(interval.cadence, ScheduleCadence::Interval(_)));
        assert_eq!(interval.jitter_seconds, 600);
        assert!(interval.paused);

        let daily = parse_schedule_settings(ScheduleMode::DailyUtc, "06:45", "5", false)
            .expect("daily schedule");
        assert!(matches!(daily.cadence, ScheduleCadence::DailyUtc(_)));
        assert_eq!(daily.jitter_seconds, 300);
        assert!(parse_schedule_settings(ScheduleMode::DailyUtc, "24:00", "0", false).is_err());
    }

    #[test]
    fn network_policy_editor_round_trips_bounded_durable_values() {
        let policy = NetworkTransferPolicy::new(8, Some(1_048_576), true).expect("policy");
        let editor = NetworkPolicyEditor::from_policy(policy);
        assert_eq!(editor.policy(), Ok(policy));

        let unlimited = NetworkPolicyEditor {
            max_concurrent_requests: "4".to_owned(),
            max_download_bytes_per_second: String::new(),
            avoid_metered_networks: false,
        };
        assert_eq!(
            unlimited
                .policy()
                .expect("unlimited policy")
                .max_download_bytes_per_second(),
            None
        );

        let invalid = NetworkPolicyEditor {
            max_concurrent_requests: "0".to_owned(),
            max_download_bytes_per_second: "0".to_owned(),
            avoid_metered_networks: true,
        };
        assert!(invalid.policy().is_err());
    }

    #[test]
    fn sync_snapshot_surfaces_durable_dump_import_progress_and_failure() {
        let temporary = tempfile::tempdir().expect("library");
        let (wiki_id, collection_id) = seeded_admin_collection(temporary.path());
        let mut library = Library::open(temporary.path()).expect("writer");
        let generation = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured")
            .generation;
        let run = library
            .start_or_resume_sync_run(wiki_id, Some(collection_id), SyncRunKind::Bootstrap, 1_000)
            .expect("bootstrap run");
        let import = library
            .claim_or_resume_dump_import(DumpImportRequest {
                run_id: run.status.run_id,
                dump_digest: &format!("b3:{}", "ab".repeat(32)),
                dump_compressed_bytes: 4_096,
                collection_generation: generation,
                bootstrap_started_at: run.status.checkpoint_candidate,
            })
            .expect("dump import");
        library
            .record_dump_import_progress(import.status.import_id, 23)
            .expect("progress");
        library
            .fail_dump_import(
                import.status.import_id,
                "fixture-interruption",
                "restartable fixture failure",
                true,
            )
            .expect("failure");

        let snapshot = snapshot(&library).expect("dashboard snapshot");
        assert_eq!(snapshot.dump_imports.len(), 1);
        let status = &snapshot.dump_imports[0];
        assert_eq!(status.collection_id, collection_id);
        assert_eq!(status.pages_scanned, 23);
        assert_eq!(status.dump_compressed_bytes, 4_096);
        assert!(status.retryable);
        assert_eq!(
            status.latest_error.as_ref().expect("error").code,
            "fixture-interruption"
        );
    }

    #[tokio::test]
    async fn confirmed_dump_start_rejects_a_scope_changed_after_preview() {
        let temporary = tempfile::tempdir().expect("library");
        let (wiki_id, collection_id) = seeded_admin_collection(temporary.path());
        let form = DumpBootstrapForm {
            collection_id: collection_id.get().to_string(),
            trusted_index_url: "https://dumps.wikimedia.org/enwiki/fixture/index.json".to_owned(),
            trusted_index_digest: "ab".repeat(32),
            expected_database: "enwiki".to_owned(),
            ..DumpBootstrapForm::default()
        };
        let preview = form.preview(temporary.path()).expect("read-only preview");

        let mut library = Library::open(temporary.path()).expect("concurrent writer");
        administer_collection_direct(
            &mut library,
            CollectionAdministration::Edit {
                collection_id,
                expected_generation: preview.collection_generation,
                draft: CollectionDraft {
                    wiki_id,
                    name: "Changed after preview".to_owned(),
                    preview: administration_preview("Ferris", 20),
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget: CollectionBudget::unlimited(),
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                },
            },
        )
        .expect("concurrent scope change");
        drop(library);

        let error = start_dump_bootstrap(temporary.path().to_path_buf(), preview)
            .await
            .expect_err("stale confirmed preview must fail before acquisition");
        assert!(error.contains("changed after the dump bootstrap preview"));
        assert!(!temporary.path().join("cache/dumps").exists());
    }

    fn administration_preview(title: &str, page_id: u64) -> CollectionSelectionPreview {
        let title = PageTitle::new(title).expect("title");
        CollectionSelectionPreview {
            rule: CollectionRule::ExplicitTitles(
                wikisync_core::TitleSelection::new([title.clone()]).expect("selection"),
            ),
            members: vec![wikisync_store::ResolvedCollectionMember {
                page_id: PageId::new(page_id).expect("page ID"),
                namespace: 0,
                title: title.clone(),
                inclusion_reason: wikisync_core::InclusionReason::ExplicitTitle(title),
            }],
            missing_titles: Vec::new(),
            predicted_canonical_bytes: Some(1_024),
            category_batches: 0,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_dump_bootstrap_succeeds_inside_the_gui_async_task_context() {
        let server = DumpFixtureServer::start();
        let temporary = tempfile::tempdir().expect("library");
        let collection_id = {
            let mut library = Library::open(temporary.path()).expect("library");
            let wiki_id = library
                .register_wiki(&server.api_endpoint, "en")
                .expect("fixture source");
            let outcome = administer_collection_direct(
                &mut library,
                CollectionAdministration::Add(CollectionDraft {
                    wiki_id,
                    name: "Async dump fixture".to_owned(),
                    preview: administration_preview("Alpha", 10),
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget: CollectionBudget::unlimited()
                        .with_maximum_pages(1)
                        .expect("page budget")
                        .with_maximum_bytes(1_024)
                        .expect("byte budget"),
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                }),
            )
            .expect("fixture collection");
            added_collection_id(outcome).expect("collection ID")
        };
        let form = DumpBootstrapForm {
            collection_id: collection_id.get().to_string(),
            trusted_index_url: server.index_url.clone(),
            trusted_index_digest: server.index_digest.clone(),
            expected_database: "enwiki".to_owned(),
            ..DumpBootstrapForm::default()
        };
        let preview = form.preview(temporary.path()).expect("dump preview");
        assert_eq!(server.requests.load(Ordering::Acquire), 0);

        let snapshot = start_dump_bootstrap(temporary.path().to_path_buf(), preview)
            .await
            .expect("async-safe direct dump bootstrap");
        assert_eq!(snapshot.dump_imports.len(), 1);
        assert_eq!(snapshot.dump_imports[0].state.as_str(), "succeeded");
        assert_eq!(snapshot.dump_imports[0].imported_pages, 1);
        assert!(temporary.path().join("cache/dumps").is_dir());
        server.finish();
    }

    fn seeded_admin_collection(path: &Path) -> (WikiId, CollectionId) {
        let mut library = Library::open(path).expect("library");
        let wiki_id = library
            .register_wiki("https://example.invalid/w/api.php", "en")
            .expect("source");
        let outcome = administer_collection_direct(
            &mut library,
            CollectionAdministration::Add(CollectionDraft {
                wiki_id,
                name: "Original".to_owned(),
                preview: administration_preview("Rust", 10),
                history_policy: HistoryPolicy::CurrentAndFuture,
                budget: CollectionBudget::unlimited(),
                removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            }),
        )
        .expect("seed collection");
        (
            wiki_id,
            added_collection_id(outcome).expect("added outcome"),
        )
    }

    fn seeded_purge_collection(path: &Path) -> CollectionId {
        let mut library = Library::open(path).expect("purge fixture library");
        let wiki_id = library
            .register_wiki("https://example.invalid/w/api.php", "en")
            .expect("purge fixture source");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Exact purge fixture")
            .expect("purge fixture collection");
        let title = PageTitle::new("Exclusive purge page").expect("purge fixture title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(91).unwrap(),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(901).unwrap(),
                    parent_id: None,
                    timestamp: "2026-08-25T08:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source: b"exclusive canonical purge fixture payload",
                },
            )
            .expect("capture purge fixture");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone purge fixture");
        collection_id
    }

    fn confirmed_purge_request(path: &Path, collection_id: CollectionId) -> CollectionPurgeRequest {
        let preview =
            preview_collection_purge(path, collection_id).expect("read-only purge preview");
        CollectionPurgeRequest {
            collection_id,
            collection_name: preview.collection_name,
            preview_fingerprint: preview.fingerprint,
            payload_only_acknowledged: true,
            external_copies_not_erased_acknowledged: true,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collection_purge_matches_direct_and_daemon_writer_paths() {
        let direct_root = tempfile::tempdir().expect("direct purge library");
        let daemon_root = tempfile::tempdir().expect("daemon purge library");
        let direct_id = seeded_purge_collection(direct_root.path());
        let daemon_id = seeded_purge_collection(daemon_root.path());
        let direct_request = confirmed_purge_request(direct_root.path(), direct_id);
        let daemon_request = confirmed_purge_request(daemon_root.path(), daemon_id);

        let handler = ApplicationHandler::new(daemon_root.path()).expect("purge daemon handler");
        let daemon = wikisyncd::Daemon::bind(daemon_root.path(), handler).expect("purge daemon");
        let shutdown = daemon.shutdown_handle();
        let daemon_thread = thread::spawn(move || daemon.run());
        wikisyncd::Client::for_library(daemon_root.path())
            .expect("purge daemon client")
            .health()
            .expect("purge daemon readiness");

        let direct = execute_collection_purge(direct_root.path().to_path_buf(), direct_request)
            .await
            .expect("direct purge");
        let forwarded = execute_collection_purge(daemon_root.path().to_path_buf(), daemon_request)
            .await
            .expect("daemon purge");
        for result in [&direct, &forwarded] {
            assert_eq!(result.outcome.progress.state, PurgeJournalState::Succeeded);
            assert!(result.outcome.progress.manifest_installed);
            assert_eq!(result.outcome.progress.pending_file_count, 0);
            assert_eq!(result.snapshot.tombstoned_collections.len(), 1);
        }
        assert_eq!(
            direct.outcome.progress.net_reclaimed_file_bytes,
            forwarded.outcome.progress.net_reclaimed_file_bytes
        );

        shutdown.shutdown();
        daemon_thread
            .join()
            .expect("purge daemon thread")
            .expect("purge daemon shutdown");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collection_edit_and_removal_match_direct_and_daemon_paths() {
        let direct_root = tempfile::tempdir().expect("direct library");
        let daemon_root = tempfile::tempdir().expect("daemon library");
        let (direct_wiki, direct_id) = seeded_admin_collection(direct_root.path());
        let (daemon_wiki, daemon_id) = seeded_admin_collection(daemon_root.path());
        assert_eq!(direct_wiki, daemon_wiki);
        assert_eq!(direct_id, daemon_id);

        let handler =
            wikisyncd::ApplicationHandler::new(daemon_root.path()).expect("application handler");
        let daemon = wikisyncd::Daemon::bind(daemon_root.path(), handler).expect("daemon");
        let shutdown = daemon.shutdown_handle();
        let daemon_thread = thread::spawn(move || daemon.run());
        let daemon_client =
            wikisyncd::Client::for_library(daemon_root.path()).expect("daemon client");
        daemon_client.health().expect("daemon readiness");

        let edit = |path: &Path, collection_id, wiki_id| EditCollectionRequest {
            library_path: path.to_path_buf(),
            collection_id,
            expected_generation: 1,
            wiki_id,
            name: "Edited".to_owned(),
            preview: administration_preview("Ferris", 20),
            history_policy: HistoryPolicy::Complete,
            budget: CollectionBudget::unlimited()
                .with_maximum_pages(50)
                .expect("page budget"),
            removal_policy: CollectionRemovalPolicy::KeepTracking,
            image_policy: ImagePolicy::Thumbnails(
                ThumbnailPolicy::new(640, 8, 1_048_576).expect("thumbnail policy"),
            ),
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::interval(7_200).expect("interval"),
                jitter_seconds: 300,
                paused: true,
            },
        };
        edit_collection(edit(direct_root.path(), direct_id, direct_wiki))
            .await
            .expect("direct edit");
        edit_collection(edit(daemon_root.path(), daemon_id, daemon_wiki))
            .await
            .expect("daemon edit");

        for root in [direct_root.path(), daemon_root.path()] {
            let library = Library::open_read_only(root).expect("inspect library");
            let configuration = library
                .collection_configuration(direct_id)
                .expect("configuration")
                .expect("configured");
            assert_eq!(configuration.name, "Edited");
            assert_eq!(configuration.history_policy, HistoryPolicy::Complete);
            assert_eq!(configuration.generation, 2);
            assert_eq!(
                configuration.removal_policy,
                CollectionRemovalPolicy::KeepTracking
            );
            assert!(matches!(
                configuration.image_policy,
                ImagePolicy::Thumbnails(_)
            ));
            let members = library
                .resolved_collection_members(direct_id)
                .expect("members");
            assert_eq!(members.len(), 2);
            assert!(members.iter().any(|member| member.title.as_str() == "Rust"));
            assert!(
                members
                    .iter()
                    .any(|member| member.title.as_str() == "Ferris")
            );
            let schedule = library
                .collection_schedule(direct_id)
                .expect("schedule")
                .expect("scheduled");
            assert!(schedule.paused);
            assert!(matches!(schedule.cadence, ScheduleCadence::Interval(_)));
        }

        remove_collection(direct_root.path().to_path_buf(), direct_id)
            .await
            .expect("direct removal");
        remove_collection(daemon_root.path().to_path_buf(), daemon_id)
            .await
            .expect("daemon removal");
        for root in [direct_root.path(), daemon_root.path()] {
            let library = Library::open_read_only(root).expect("inspect tombstone");
            assert!(
                library
                    .collections()
                    .expect("active collections")
                    .is_empty()
            );
            let retained = library
                .collections_including_tombstones()
                .expect("retained collections");
            assert_eq!(retained.len(), 1);
            assert_eq!(retained[0].status.as_str(), "tombstoned");
        }

        shutdown.shutdown();
        daemon_thread
            .join()
            .expect("daemon thread")
            .expect("daemon shutdown");
    }

    #[tokio::test]
    async fn stale_gui_edit_is_rejected_with_reload_and_repreview_guidance() {
        let temporary = tempfile::tempdir().expect("library");
        let (wiki_id, collection_id) = seeded_admin_collection(temporary.path());
        let stale_request = EditCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            collection_id,
            expected_generation: 1,
            wiki_id,
            name: "Stale GUI edit".to_owned(),
            preview: administration_preview("Ferris", 20),
            history_policy: HistoryPolicy::Complete,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::Thumbnails(
                ThumbnailPolicy::new(320, 4, 524_288).expect("thumbnail policy"),
            ),
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        };
        let mut library = Library::open(temporary.path()).expect("concurrent writer");
        administer_collection_direct(
            &mut library,
            CollectionAdministration::Edit {
                collection_id,
                expected_generation: 1,
                draft: CollectionDraft {
                    wiki_id,
                    name: "Concurrent edit".to_owned(),
                    preview: administration_preview("Rust", 10),
                    history_policy: HistoryPolicy::CurrentAndFuture,
                    budget: CollectionBudget::unlimited(),
                    removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
                },
            },
        )
        .expect("concurrent edit");
        drop(library);

        let error = edit_collection(stale_request)
            .await
            .expect_err("stale preview must fail");
        assert!(error.contains("Reload the collection"));
        assert!(error.contains("preview it again"));
        let library = Library::open_read_only(temporary.path()).expect("inspect library");
        let configuration = library
            .collection_configuration(collection_id)
            .expect("configuration")
            .expect("configured");
        assert_eq!(configuration.name, "Concurrent edit");
        assert_eq!(configuration.generation, 2);
        assert_eq!(configuration.image_policy, ImagePolicy::None);
    }

    #[tokio::test]
    async fn direct_create_reports_registered_source_when_collection_commit_fails() {
        let temporary = tempfile::tempdir().expect("library");
        Library::open(temporary.path()).expect("initialize library");
        let error = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Over budget".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: "https://example.invalid/w/api.php".to_owned(),
            preview: administration_preview("Rust", 10),
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited()
                .with_maximum_bytes(1)
                .expect("byte budget"),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::None,
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        })
        .await
        .expect_err("collection must exceed budget");
        assert!(error.contains("was registered successfully"));
        assert!(error.contains("remains configured and can be reused"));
        let library = Library::open_read_only(temporary.path()).expect("inspect library");
        assert_eq!(library.wikis().expect("sources").len(), 1);
        assert!(library.collections().expect("collections").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_create_reports_registered_source_when_collection_commit_fails() {
        let temporary = tempfile::tempdir().expect("library");
        Library::open(temporary.path()).expect("initialize library");
        let handler =
            wikisyncd::ApplicationHandler::new(temporary.path()).expect("application handler");
        let daemon = wikisyncd::Daemon::bind(temporary.path(), handler).expect("daemon");
        let shutdown = daemon.shutdown_handle();
        let daemon_thread = thread::spawn(move || daemon.run());
        wikisyncd::Client::for_library(temporary.path())
            .expect("daemon client")
            .health()
            .expect("daemon readiness");

        let error = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Over budget".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: "https://example.invalid/w/api.php".to_owned(),
            preview: administration_preview("Rust", 10),
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited()
                .with_maximum_bytes(1)
                .expect("byte budget"),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::None,
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        })
        .await
        .expect_err("collection must exceed budget");
        assert!(error.contains("was registered successfully"));
        assert!(error.contains("remains configured and can be reused"));
        let library = Library::open_read_only(temporary.path()).expect("inspect library");
        assert_eq!(library.wikis().expect("sources").len(), 1);
        assert!(library.collections().expect("collections").is_empty());
        drop(library);

        shutdown.shutdown();
        daemon_thread
            .join()
            .expect("daemon thread")
            .expect("daemon shutdown");
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

    #[test]
    fn trust_artifact_paths_must_be_absolute_and_outside_the_library() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let library_path = temporary.path().join("library");
        let trust_path = temporary.path().join("trust");
        fs::create_dir_all(&library_path).expect("library directory");
        fs::create_dir_all(&trust_path).expect("trust directory");

        assert!(
            explicit_artifact_path(
                library_path.to_str().unwrap(),
                "relative-key.pk8",
                "Signing key"
            )
            .is_err()
        );
        assert!(
            explicit_artifact_path(
                library_path.to_str().unwrap(),
                library_path.join("key.pk8").to_str().unwrap(),
                "Signing key"
            )
            .is_err()
        );
        assert_eq!(
            explicit_artifact_path(
                library_path.to_str().unwrap(),
                trust_path.join("key.pk8").to_str().unwrap(),
                "Signing key"
            )
            .unwrap(),
            trust_path.canonicalize().unwrap().join("key.pk8")
        );
    }

    #[test]
    fn signing_key_generation_is_private_validated_and_non_overwriting() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("signing-key.pk8");

        generate_signing_key(&path).expect("generate signing key");
        load_signing_key(&path).expect("validate signing key");
        let original = fs::read(&path).expect("read signing key");
        assert!(generate_signing_key(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn external_anchor_refresh_and_full_comparison_round_trip() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let library_path = temporary.path().join("library");
        let trust_path = temporary.path().join("trust");
        fs::create_dir(&trust_path).expect("trust directory");
        let mut library = Library::open(&library_path).expect("library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("wiki");
        let run_id = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Bootstrap, 100)
            .expect("start run")
            .status
            .run_id;
        library
            .complete_sync_run(run_id, None)
            .expect("complete run");
        library.append_sync_manifest(run_id).expect("manifest");
        drop(library);

        let signing_key_path = trust_path.join("signing-key.pk8");
        let trusted_head_path = trust_path.join("trusted-head.json");
        generate_signing_key(&signing_key_path).expect("signing key");
        let refreshed =
            refresh_trusted_head(library_path.clone(), &signing_key_path, &trusted_head_path)
                .await
                .expect("refresh trusted head");
        assert!(refreshed.report.is_verified_since_capture());
        assert!(trusted_head_path.is_file());

        let first_anchor = load_trusted_head(&trusted_head_path).expect("first anchor");
        let previous_path = previous_anchor_path(
            &trusted_head_path,
            first_anchor.sequence,
            first_anchor.public_key(),
        )
        .expect("previous path");
        let mut library = Library::open(&library_path).expect("advanced library");
        let second_run = library
            .start_or_resume_sync_run(wiki_id, None, SyncRunKind::Update, 200)
            .expect("start second run")
            .status
            .run_id;
        library
            .complete_sync_run(second_run, None)
            .expect("complete second run");
        library
            .append_sync_manifest(second_run)
            .expect("second manifest");
        drop(library);
        refresh_trusted_head(library_path.clone(), &signing_key_path, &trusted_head_path)
            .await
            .expect("refresh advanced trusted head");
        assert!(previous_path.is_file());

        let older_comparison = verify_against_trusted_head(library_path.clone(), &previous_path)
            .await
            .expect("compare retained older head");
        assert!(
            older_comparison
                .findings
                .iter()
                .any(|finding| finding.kind == VerificationFindingKind::TrustedHeadMismatch)
        );

        let compared = verify_against_trusted_head(library_path, &trusted_head_path)
            .await
            .expect("compare trusted head");
        assert!(compared.is_authenticated_against_trusted_head());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_owned_create_registers_new_source_then_bootstraps_fixture_collection() {
        let server = FixtureServer::start(vec![TITLE_RESOLUTION, UNCHANGED_HEAD, REVISION_CONTENT]);
        let temporary = tempfile::tempdir().expect("temporary library");
        Library::open(temporary.path()).expect("library");

        let preview = preview_collection(PreviewCollectionRequest {
            api_endpoint: server.endpoint.clone(),
            network_policy: NetworkTransferPolicy::default(),
            rule: CollectionRule::ExplicitTitles(
                wikisync_core::TitleSelection::new([
                    PageTitle::new("Rust_programming_language").expect("title")
                ])
                .expect("selection"),
            ),
        })
        .await
        .expect("preview");

        let handler =
            wikisyncd::ApplicationHandler::new(temporary.path()).expect("application handler");
        let daemon = wikisyncd::Daemon::bind(temporary.path(), handler).expect("daemon");
        let shutdown = daemon.shutdown_handle();
        let daemon_thread = thread::spawn(move || daemon.run());
        wikisyncd::Client::for_library(temporary.path())
            .expect("daemon client")
            .health()
            .expect("daemon readiness");

        let snapshot = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Daemon collection".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: server.endpoint.clone(),
            preview,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::Thumbnails(
                ThumbnailPolicy::new(800, 12, 2 * 1024 * 1024).expect("thumbnail policy"),
            ),
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        })
        .await
        .expect("daemon create and bootstrap");
        assert_eq!(snapshot.collections.len(), 1);
        assert_eq!(snapshot.collections[0].page_count, 1);
        assert_eq!(snapshot.collection_configurations.len(), 1);
        assert_eq!(snapshot.collection_configurations[0].generation, 1);
        assert!(matches!(
            snapshot.collection_configurations[0].image_policy,
            ImagePolicy::Thumbnails(policy)
                if policy.maximum_edge_pixels().get() == 800
                    && policy.maximum_images_per_revision().get() == 12
                    && policy.maximum_bytes_per_image().get() == 2 * 1024 * 1024
        ));
        assert_eq!(snapshot.wikis.len(), 1);
        assert_eq!(snapshot.wikis[0].api_endpoint, server.endpoint);
        assert_eq!(snapshot.wikis[0].language_code, "en");
        assert!(
            snapshot
                .runs
                .iter()
                .any(|run| run.state == SyncRunState::Succeeded)
        );

        shutdown.shutdown();
        daemon_thread
            .join()
            .expect("daemon thread")
            .expect("daemon shutdown");
        assert_eq!(server.finish().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_then_daemon_multilanguage_lifecycle_is_durable_and_source_isolated() {
        let english_server =
            FixtureServer::start(vec![TITLE_RESOLUTION, UNCHANGED_HEAD, REVISION_CONTENT]);
        let norwegian_server = FixtureServer::start(vec![
            NORWEGIAN_TITLE_RESOLUTION,
            NORWEGIAN_UNCHANGED_HEAD,
            NORWEGIAN_REVISION_CONTENT,
        ]);
        let temporary = tempfile::tempdir().expect("temporary library");
        load_library_snapshot(temporary.path(), true).expect("create library through GUI flow");

        let english_preview = preview_collection(PreviewCollectionRequest {
            api_endpoint: english_server.endpoint.clone(),
            network_policy: NetworkTransferPolicy::default(),
            rule: CollectionRule::ExplicitTitles(
                wikisync_core::TitleSelection::new([
                    PageTitle::new("Rust_programming_language").expect("English title")
                ])
                .expect("English selection"),
            ),
        })
        .await
        .expect("preview English fixture");
        let direct_snapshot = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "English systems language".to_owned(),
            language_code: "en".to_owned(),
            api_endpoint: english_server.endpoint.clone(),
            preview: english_preview,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::None,
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        })
        .await
        .expect("direct English create and bootstrap");
        assert_eq!(direct_snapshot.wikis.len(), 1);
        assert_eq!(direct_snapshot.collections.len(), 1);

        let norwegian_preview = preview_collection(PreviewCollectionRequest {
            api_endpoint: norwegian_server.endpoint.clone(),
            network_policy: NetworkTransferPolicy::default(),
            rule: CollectionRule::ExplicitTitles(
                wikisync_core::TitleSelection::new([
                    PageTitle::new("Rust_programmeringssprak").expect("Norwegian title")
                ])
                .expect("Norwegian selection"),
            ),
        })
        .await
        .expect("preview Norwegian fixture");

        let handler =
            wikisyncd::ApplicationHandler::new(temporary.path()).expect("application handler");
        let daemon = wikisyncd::Daemon::bind(temporary.path(), handler).expect("daemon");
        let shutdown = daemon.shutdown_handle();
        let daemon_thread = thread::spawn(move || daemon.run());
        wikisyncd::Client::for_library(temporary.path())
            .expect("daemon client")
            .health()
            .expect("daemon readiness");

        let daemon_snapshot = create_collection_and_sync(&CreateCollectionRequest {
            library_path: temporary.path().to_path_buf(),
            name: "Norsk systemsprak".to_owned(),
            language_code: "nb".to_owned(),
            api_endpoint: norwegian_server.endpoint.clone(),
            preview: norwegian_preview,
            history_policy: HistoryPolicy::CurrentAndFuture,
            budget: CollectionBudget::unlimited(),
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::None,
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::Manual,
                jitter_seconds: 0,
                paused: false,
            },
        })
        .await
        .expect("daemon-owned Norwegian create and bootstrap");
        assert_eq!(daemon_snapshot.wikis.len(), 2);
        assert_eq!(daemon_snapshot.collections.len(), 2);
        assert_eq!(daemon_snapshot.unique_page_count, 2);

        shutdown.shutdown();
        daemon_thread
            .join()
            .expect("daemon thread")
            .expect("daemon shutdown");
        assert_eq!(english_server.finish().len(), 3);
        assert_eq!(norwegian_server.finish().len(), 3);

        let durable_snapshot =
            load_library_snapshot(temporary.path(), false).expect("reopen durable GUI snapshot");
        let english_wiki = durable_snapshot
            .wikis
            .iter()
            .find(|wiki| wiki.language_code == "en")
            .expect("durable English source");
        let norwegian_wiki = durable_snapshot
            .wikis
            .iter()
            .find(|wiki| wiki.language_code == "nb")
            .expect("durable Norwegian source");
        assert_ne!(english_wiki.wiki_id, norwegian_wiki.wiki_id);
        assert_ne!(english_wiki.api_endpoint, norwegian_wiki.api_endpoint);
        let english_collection = durable_snapshot
            .collections
            .iter()
            .find(|collection| collection.name == "English systems language")
            .expect("durable English collection");
        let norwegian_collection = durable_snapshot
            .collections
            .iter()
            .find(|collection| collection.name == "Norsk systemsprak")
            .expect("durable Norwegian collection");
        assert_eq!(english_collection.wiki_id, english_wiki.wiki_id);
        assert_eq!(norwegian_collection.wiki_id, norwegian_wiki.wiki_id);
        assert_eq!(durable_snapshot.unique_page_count, 2);
        assert_eq!(durable_snapshot.recent_revisions.len(), 2);

        let library = Library::open_read_only(temporary.path()).expect("inspect durable library");
        let english_page = library
            .collection_pages(english_wiki.wiki_id, english_collection.collection_id)
            .expect("English collection pages")
            .remove(0);
        let norwegian_page = library
            .collection_pages(norwegian_wiki.wiki_id, norwegian_collection.collection_id)
            .expect("Norwegian collection pages")
            .remove(0);
        assert_eq!(
            english_page.page_id, norwegian_page.page_id,
            "the fixture intentionally collides upstream page IDs across sources"
        );
        assert_ne!(english_page.title, norwegian_page.title);
        assert!(
            library
                .collection_pages(norwegian_wiki.wiki_id, english_collection.collection_id)
                .is_err(),
            "a collection cannot be inspected through another source identity"
        );
        assert!(
            library
                .collection_pages(english_wiki.wiki_id, norwegian_collection.collection_id)
                .is_err(),
            "source isolation is symmetric"
        );

        let english_revision = library
            .revisions_for_page(english_wiki.wiki_id, english_page.page_id)
            .expect("English revisions")
            .remove(0);
        let norwegian_revision = library
            .revisions_for_page(norwegian_wiki.wiki_id, norwegian_page.page_id)
            .expect("Norwegian revisions")
            .remove(0);
        assert_eq!(
            english_revision.revision_id, norwegian_revision.revision_id,
            "the fixture intentionally collides upstream revision IDs across sources"
        );
        assert_eq!(
            library
                .read_object(english_revision.content_object_id)
                .expect("durable English content"),
            b"== Rust ==\nA systems programming language."
        );
        assert_eq!(
            library
                .read_object(norwegian_revision.content_object_id)
                .expect("durable Norwegian content"),
            b"== Rust ==\nEt systemprogrammeringssprak."
        );
        assert_ne!(
            english_revision.content_object_id,
            norwegian_revision.content_object_id
        );
        assert!(
            verify_library(&library, VerificationScope::Full)
                .expect("verify multi-source library")
                .is_verified_since_capture()
        );
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
            network_policy: NetworkTransferPolicy::default(),
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
            removal_policy: CollectionRemovalPolicy::StopTrackingRetainHistory,
            image_policy: ImagePolicy::None,
            schedule: ScheduleSettings {
                cadence: ScheduleCadence::interval(3_600).unwrap(),
                jitter_seconds: 300,
                paused: false,
            },
        })
        .await
        .expect("create and sync");
        assert_eq!(created.collections[0].page_count, 1);
        let collection_id = created.collections[0].collection_id;
        assert!(matches!(
            created.schedules[0].cadence,
            ScheduleCadence::Interval(_)
        ));

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
