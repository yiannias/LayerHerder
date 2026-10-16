#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use acad_layer_core::{
    HeuristicMatcher, LayerDictionaryDefinition, MatchResult, MatchSource, MemoryStore,
    PluginConfig,
};
use acad_layer_ipc::{
    DrawingLayersInfo, DrawingSnapshot, FeedHandle, FeedMessage, IpcResponse, PendingEntry,
    TargetFilter,
};
use live_sync::LayerRead;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::{Deserialize, Serialize};
use spatial_ui_kit::theme::ThemePalette;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    sync::{Arc, Mutex},
    time::Instant,
};

mod close_dialog;
mod data;

mod live_sync;
mod mapping_editor;
mod sessions;
mod settings_panel;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct UserPreferences {
    mapping_editor_width: f32,
    mapping_editor_height: f32,
    mapping_editor_maximized: bool,
    mapping_editor_zoom: f32,
    mapping_editor_viewport_x: f32,
    mapping_editor_viewport_y: f32,
    animations_enabled: bool,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            mapping_editor_width: 1200.0,
            mapping_editor_height: 650.0,
            mapping_editor_maximized: false,
            mapping_editor_zoom: 1.0,
            mapping_editor_viewport_x: 0.0,
            mapping_editor_viewport_y: 0.0,
            animations_enabled: true,
        }
    }
}

fn preferences_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("AcLayerStandardizer").join("ui_preferences.json"))
}

fn load_preferences() -> UserPreferences {
    preferences_path()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Everything worker threads post to the UI thread's result channel.
enum AppMessage {
    /// A request the user started: the first snapshot, a standard, Apply or Purge.
    Ipc(Result<IpcResponse, String>),
    /// A live-sync layer read (switch, refresh or resync).
    Layers(Result<IpcResponse, String>),
    /// A message from the event feed thread.
    Feed(FeedMessage),
    /// The answer to replaying a close or quit AutoCAD blocked.
    Replay(Result<IpcResponse, String>),
    /// Whether the Standards File exists, checked off the UI thread for the
    /// Settings panel.
    StandardExists { path: String, exists: bool },
}

/// Asks which drawing is active, then reads its layers (the feed's resync path).
fn read_active_drawing_layers() -> Result<IpcResponse, String> {
    match acad_layer_ipc::get_active_drawing(None)? {
        IpcResponse::ActiveDrawing(info) => acad_layer_ipc::get_layers_for_drawing(info.drawing_id),
        other => Ok(other),
    }
}

/// The longest the window's exit waits for the feed's last report.
const FEED_EXIT_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Template drawings the user can choose as the standard.
const STANDARD_FILE_EXTENSIONS: [&str; 3] = ["dwg", "dxf", "dws"];

/// Checks the file exists (off the UI thread: the template is often on a network
/// drive that may be slow or unreachable) before asking AutoCAD to read it.
fn fetch_standard(path: String) -> Result<IpcResponse, String> {
    if !std::path::Path::new(&path).exists() {
        return Err(format!(
            "Reference file unavailable: {path}. Click the Target header to choose one."
        ));
    }
    acad_layer_ipc::get_standard_layers(path)
}

#[allow(dead_code)]
struct LayerStandardizerApp {
    dark_theme: ThemePalette,
    search_query: String,
    min_confidence: f64,
    source_layers: Vec<String>,
    standard_layers: Vec<String>,
    matches: Vec<MatchResult>,
    status_message: String,
    error_message: Option<String>,
    /// When set, dismissing the error dialog also closes the window.
    close_after_error: bool,
    owner_hwnd: Option<isize>,
    owner_attached: bool,
    apply_pending: bool,
    ipc_sender: Sender<AppMessage>,
    ipc_results: Receiver<AppMessage>,
    /// The event feed; started once the first drawing snapshot arrives.
    feed: Option<FeedHandle>,
    /// Edit states of open drawings that are not displayed.
    sessions: sessions::DrawingSessions,
    /// Unapplied counts the feed reports to the connector on every poll.
    pending_report: Arc<Mutex<Vec<PendingEntry>>>,
    connection_lost: bool,
    /// Layer reads still wanted (`reads.refresh_wanted()`); they wait while an
    /// Apply, Purge or Load Standard is in flight.
    reads: live_sync::ReadSchedule,
    /// The displayed drawing closed (or none is active): "No drawing is open".
    no_drawing: bool,
    drawing_name: String,
    drawing_id: String,
    template_name: String,
    empty_layers: HashSet<String>,
    memory_mappings: HashMap<String, String>,
    target_filters: Vec<TargetFilter>,
    always_hidden_targets: HashSet<String>,
    plugin_config: PluginConfig,
    config_path: Option<PathBuf>,
    config_writable: bool,
    dictionary: LayerDictionaryDefinition,
    memory_store: Option<MemoryStore>,
    standard_requested: bool,
    /// Source layers and mapped targets to remember once AutoCAD confirms the Apply.
    pending_remember: Option<(Vec<String>, HashMap<String, String>)>,
    mapping_editor: mapping_editor::MappingEditor,
    user_preferences: UserPreferences,
    /// Apply / Discard / Cancel dialogs: the shown one and any waiting their turn.
    close_dialogs: close_dialog::CloseQueue,
    /// The window's own close was approved (or needs no dialog): let it through.
    close_approved: bool,
    /// The close dialog whose Apply is in flight; on `Applied` a blocked close or
    /// quit is replayed.
    apply_for_close: Option<close_dialog::CloseReason>,
    /// Close the window once the replay was queued (Apply from a blocked close).
    close_after_replay: bool,
    settings: settings_panel::SettingsPanel,
    /// Whether the Standards File exists; `None` while the check is running.
    standard_file_exists: Option<bool>,
    /// The beta notice banner (from `--first-run-notice`); never persisted.
    notice_visible: bool,
}

impl LayerStandardizerApp {
    fn new(
        owner_hwnd: Option<isize>,
        ipc_sender: Sender<AppMessage>,
        ipc_results: Receiver<AppMessage>,
        user_preferences: UserPreferences,
        first_run_notice: bool,
    ) -> Self {
        let startup = data::load_startup();
        let min_confidence = startup.config.heuristic_threshold.clamp(0.6, 1.0);
        let mut mapping_editor = mapping_editor::MappingEditor::default();
        mapping_editor.confidence = min_confidence;
        mapping_editor.restore_view_preferences(
            user_preferences.mapping_editor_zoom,
            user_preferences.mapping_editor_viewport_x,
            user_preferences.mapping_editor_viewport_y,
            user_preferences.animations_enabled,
        );
        Self {
            dark_theme: ThemePalette::dark(),
            search_query: String::new(),
            min_confidence,
            source_layers: Vec::new(),
            standard_layers: Vec::new(),
            matches: Vec::new(),
            status_message: if owner_hwnd.is_some() {
                "Connecting to AutoCAD…".to_string()
            } else {
                "Ready. Connect AutoCAD or paste/import layers to standardize.".to_string()
            },
            owner_hwnd,
            owner_attached: false,
            apply_pending: false,
            close_after_error: false,
            error_message: (!startup.notes.is_empty()).then(|| startup.notes.join("\n\n")),
            ipc_sender,
            ipc_results,
            feed: None,
            sessions: sessions::DrawingSessions::default(),
            pending_report: Arc::new(Mutex::new(Vec::new())),
            connection_lost: false,
            reads: live_sync::ReadSchedule::default(),
            no_drawing: false,
            drawing_name: String::new(),
            drawing_id: String::new(),
            template_name: String::new(),
            empty_layers: HashSet::new(),
            memory_mappings: startup.memory.mappings,
            target_filters: Vec::new(),
            always_hidden_targets: HashSet::new(),
            plugin_config: startup.config,
            config_path: startup.config_path,
            config_writable: startup.config_writable,
            dictionary: startup.dictionary,
            memory_store: startup.memory_store,
            standard_requested: false,
            pending_remember: None,
            mapping_editor,
            user_preferences,
            close_dialogs: close_dialog::CloseQueue::default(),
            close_approved: false,
            apply_for_close: None,
            close_after_replay: false,
            settings: settings_panel::SettingsPanel::default(),
            standard_file_exists: None,
            notice_visible: first_run_notice,
        }
    }

    #[allow(dead_code)]
    pub fn set_layers(&mut self, source: Vec<String>, standard: Vec<String>) {
        self.source_layers = source;
        self.standard_layers = standard;
        self.recalculate();
    }

    pub fn recalculate(&mut self) {
        if self.standard_layers.is_empty() || self.source_layers.is_empty() {
            self.matches.clear();
            return;
        }

        self.matches = compute_matches(
            &self.source_layers,
            &self.standard_layers,
            &self.memory_mappings,
            self.min_confidence,
        );
    }
}

/// Exact name, then remembered mapping, then heuristic. Names compare ignoring case
/// the way the C# `OrdinalIgnoreCase` dictionaries do, so non-ASCII letters count too.
fn compute_matches(
    source_layers: &[String],
    standard_layers: &[String],
    memory_mappings: &HashMap<String, String>,
    min_confidence: f64,
) -> Vec<MatchResult> {
    let matcher = HeuristicMatcher::new(standard_layers.to_vec(), min_confidence);
    let standards_by_name = standard_layers
        .iter()
        .map(|name| (name.to_lowercase(), name.as_str()))
        .collect::<HashMap<_, _>>();
    let memory_by_source = memory_mappings
        .iter()
        .map(|(source, target)| (source.to_lowercase(), target.as_str()))
        .collect::<HashMap<_, _>>();
    source_layers
        .iter()
        .map(|name| {
            let lower = name.to_lowercase();
            if let Some(target) = standards_by_name.get(&lower) {
                return MatchResult {
                    source_layer: name.clone(),
                    target_layer: Some((*target).to_string()),
                    confidence: 1.0,
                    source: MatchSource::Exact,
                };
            }
            if let Some(target) = memory_by_source.get(&lower) {
                if let Some(target) = standards_by_name.get(&target.to_lowercase()) {
                    return MatchResult {
                        source_layer: name.clone(),
                        target_layer: Some((*target).to_string()),
                        confidence: 1.0,
                        source: MatchSource::Memory,
                    };
                }
            }
            matcher.try_match(name).unwrap_or(MatchResult {
                source_layer: name.clone(),
                target_layer: None,
                confidence: 0.0,
                source: MatchSource::Unmatched,
            })
        })
        .collect()
}

impl eframe::App for LayerStandardizerApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        ui.ctx().set_visuals(self.dark_theme.base.visuals());
        self.attach_to_owner_if_requested(frame, ui.ctx());
        while let Ok(message) = self.ipc_results.try_recv() {
            let result = match message {
                AppMessage::Ipc(result) => result,
                AppMessage::Layers(result) => {
                    self.on_layer_read(result);
                    continue;
                }
                AppMessage::Feed(message) => {
                    self.on_feed_message(ui.ctx(), message);
                    continue;
                }
                AppMessage::Replay(result) => {
                    self.on_replay(ui.ctx(), result);
                    continue;
                }
                AppMessage::StandardExists { path, exists } => {
                    // An answer for a standard that has since changed is stale.
                    if path == self.plugin_config.template_dwg_path {
                        self.standard_file_exists = Some(exists);
                    }
                    continue;
                }
            };
            match result {
                Ok(IpcResponse::DrawingSnapshot(snapshot)) => {
                    self.set_snapshot(snapshot);
                    self.status_message = format!(
                        "Connected to {} • {} drawing layers",
                        self.drawing_name,
                        self.source_layers.len()
                    );
                    self.request_standard_if_needed(ui.ctx());
                    self.start_feed(ui.ctx());
                }
                Ok(IpcResponse::StandardLayers(info)) => {
                    self.apply_pending = false;
                    self.template_name = info.template_name.clone();
                    let view = data::build_target_view(&info.layers, &self.dictionary);
                    self.target_filters = view.filters;
                    self.always_hidden_targets = view.always_hidden.into_iter().collect();
                    self.mapping_editor.retain_valid_targets(&info.layers);
                    self.sessions.retain_valid_targets(&info.layers);
                    let source = std::mem::take(&mut self.source_layers);
                    self.set_layers(source, info.layers);
                    self.status_message = format!(
                        "Connected to {} • {} drawing layers • {} standard layers",
                        self.drawing_name,
                        self.source_layers.len(),
                        self.standard_layers.len()
                    );
                    if !info
                        .template_path
                        .eq_ignore_ascii_case(&self.plugin_config.template_dwg_path)
                    {
                        self.remember_template_path(info.template_path);
                    }
                    if self.settings.open {
                        self.check_standard_file(ui.ctx());
                    }
                }
                Ok(IpcResponse::Applied {
                    protocol_version,
                    count,
                    remembered,
                    warning,
                }) => {
                    self.apply_pending = false;
                    let reason = self.apply_for_close.take();
                    // A plain Apply keeps the window open while other drawings still
                    // hold unapplied connections; closing would lose them unasked.
                    let close_after = close_dialog::close_after_applied(
                        reason.as_ref(),
                        !self
                            .sessions
                            .others_pending(self.current_drawing())
                            .is_empty(),
                    );
                    let mut keep_open = false;
                    if protocol_version != acad_layer_ipc::IPC_PROTOCOL_VERSION {
                        self.status_message = format!(
                            "AutoCAD returned unsupported apply response version {protocol_version}."
                        );
                    } else {
                        let outcome = match self.pending_remember.take() {
                            None if remembered => data::MemoryOutcome::Saved,
                            None => data::MemoryOutcome::NotRequested,
                            Some((sources, mapped)) => match &self.memory_store {
                                Some(store) => {
                                    match data::remember_mappings(store, &sources, &mapped) {
                                        Ok(()) => data::MemoryOutcome::Saved,
                                        Err(error) => data::MemoryOutcome::Failed(error),
                                    }
                                }
                                None => data::MemoryOutcome::Failed(
                                    "no translation memory location is available".to_string(),
                                ),
                            },
                        };
                        let outcome = match (warning, outcome) {
                            (Some(warning), _) => data::MemoryOutcome::Failed(warning),
                            (None, outcome) => outcome,
                        };
                        let memory_failed = !data::close_after_apply(&outcome);
                        self.status_message = data::applied_status_message(count, outcome);
                        if !close_after {
                            // The displayed drawing's connections are in the drawing
                            // now, so they no longer count as unapplied; its layers
                            // refresh through the feed.
                            drop(self.mapping_editor.take_edit_state());
                            self.mapping_editor.clear_selection();
                            if memory_failed {
                                self.error_message = Some(self.status_message.clone());
                            }
                            self.status_message =
                                close_dialog::stay_open_status(&self.status_message);
                        } else if memory_failed {
                            keep_open = true;
                            // The Apply itself succeeded, so the window is stale: keep it
                            // up only so the user sees the warning, with Apply and Purge
                            // disabled, then close it when they press OK.
                            self.error_message = Some(self.status_message.clone());
                            self.close_after_error = true;
                            self.apply_pending = true;
                        }
                    }
                    let mut replaying = false;
                    if let Some(reason) = reason {
                        if reason != close_dialog::CloseReason::WindowClose {
                            // The connections are in the drawing now: clear them (and, for
                            // a quit, the other drawings', as the dialog said) so the
                            // replayed close passes the connector's check.
                            self.discard_for_close(&reason);
                            self.replay_close(ui.ctx(), &reason);
                            replaying = true;
                        }
                    }
                    if close_after && !keep_open {
                        if replaying {
                            // Closing now would end the process before the replay is sent.
                            self.close_after_replay = true;
                        } else {
                            self.close_window(ui.ctx());
                        }
                    }
                }
                Ok(IpcResponse::Purged {
                    protocol_version,
                    layers,
                }) => {
                    self.apply_pending = false;
                    if protocol_version != acad_layer_ipc::IPC_PROTOCOL_VERSION {
                        self.status_message = format!(
                            "AutoCAD returned unsupported purge response version {protocol_version}."
                        );
                    } else {
                        self.source_layers.retain(|name| {
                            !layers
                                .iter()
                                .any(|removed| removed.eq_ignore_ascii_case(name))
                        });
                        self.empty_layers.retain(|name| {
                            !layers
                                .iter()
                                .any(|removed| removed.eq_ignore_ascii_case(name))
                        });
                        self.mapping_editor.overrides.retain(|source, _| {
                            !layers
                                .iter()
                                .any(|removed| removed.eq_ignore_ascii_case(source))
                        });
                        self.recalculate();
                        self.status_message = format!("Purged {} empty layer(s).", layers.len());
                    }
                }
                Ok(IpcResponse::Error(message)) | Err(message) => {
                    self.apply_pending = false;
                    self.status_message = message.clone();
                    self.error_message = Some(message);
                    // An Apply chosen in a close dialog failed: ask again after the error.
                    if let Some(reason) = self.apply_for_close.take() {
                        self.close_dialogs.offer(reason);
                    }
                }
                Ok(_) => {
                    self.status_message = "AutoCAD returned an unexpected IPC response.".to_string()
                }
            }
        }

        if ui.ctx().input(|input| input.viewport().close_requested())
            && !self.close_approved
            && close_dialog::should_intercept_close(
                self.mapping_editor.unapplied_count(),
                &self.sessions.others_pending(self.current_drawing()),
            )
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_dialogs
                .offer(close_dialog::CloseReason::WindowClose);
        }

        self.drive_live_sync(ui.ctx());

        let previous_threshold = self.mapping_editor.confidence;
        let footer_note = self.sessions.footer_note(self.current_drawing());
        let mut editor_event = self.mapping_editor.show(
            ui,
            &ui.ctx().clone(),
            if self.no_drawing {
                "No drawing is open"
            } else {
                &self.drawing_name
            },
            &self.template_name,
            &self.source_layers,
            &self.standard_layers,
            &self.matches,
            &self.empty_layers,
            &self.target_filters,
            &self.always_hidden_targets,
            &self.status_message,
            footer_note.as_deref(),
            self.apply_pending || self.no_drawing,
        );
        if editor_event.remember.is_some() || editor_event.purge {
            // Apply and Purge target the displayed drawing; the connector refuses one
            // that is no longer active, so say so plainly instead of sending them.
            if let Some(message) = live_sync::blocked_action_message(
                self.no_drawing,
                self.reads.switch_pending(&self.drawing_id),
            ) {
                editor_event.remember = None;
                editor_event.purge = false;
                self.status_message = message.to_string();
                self.error_message = Some(message.to_string());
            }
        }
        if let Some(new_threshold) = editor_event.confidence {
            self.mapping_editor.confidence = new_threshold;
        }
        if (self.mapping_editor.confidence - previous_threshold).abs() > f64::EPSILON {
            self.min_confidence = self.mapping_editor.confidence;
            self.recalculate();
        }

        if let Some(remember) = editor_event.remember {
            self.start_apply(ui.ctx(), remember);
        }

        if editor_event.purge {
            let request = (
                self.drawing_name.clone(),
                self.drawing_id.clone(),
                self.empty_layers.iter().cloned().collect::<Vec<_>>(),
            );
            let sender = self.ipc_sender.clone();
            let repaint = ui.ctx().clone();
            self.apply_pending = true;
            self.status_message = "Removing empty layers in AutoCAD…".to_string();
            std::thread::spawn(move || {
                let result = acad_layer_ipc::purge_empty_layers(request.0, request.1, request.2);
                let _ = sender.send(AppMessage::Ipc(result));
                repaint.request_repaint();
            });
        }

        if editor_event.choose_standard && !self.apply_pending {
            self.choose_standard_file(ui.ctx(), frame);
        }

        if editor_event.open_settings {
            if !self.settings.open {
                self.settings.status.clear();
            }
            self.settings.open = true;
            self.check_standard_file(ui.ctx());
        }
        self.show_notice_banner(ui.ctx());
        if let Some(action) = self.show_settings_panel(ui.ctx()) {
            self.handle_settings_action(ui.ctx(), frame, action);
        }

        if let Some(message) = self.error_message.clone() {
            egui::Window::new("Layer Herder")
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Tooltip)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(420.0);
                    ui.label(message);
                    if self.close_after_error {
                        ui.label("This window will close when you press OK.");
                    }
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        self.error_message = None;
                        if self.close_after_error {
                            self.close_window(ui.ctx());
                        }
                    }
                });
        } else {
            self.show_close_dialog(ui.ctx());
        }

        self.capture_user_preferences(ui.ctx());
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let Some(path) = preferences_path() else {
            return;
        };
        let Some(directory) = path.parent() else {
            return;
        };
        if fs::create_dir_all(directory).is_ok() {
            if let Ok(json) = serde_json::to_vec_pretty(&self.user_preferences) {
                let _ = fs::write(path, json);
            }
        }
    }
}

impl LayerStandardizerApp {
    fn capture_user_preferences(&mut self, ctx: &egui::Context) {
        let viewport = ctx.input(|input| input.viewport().clone());
        let maximized = viewport
            .maximized
            .unwrap_or(self.user_preferences.mapping_editor_maximized);
        if !maximized {
            if let Some(size) = viewport.inner_rect.map(|rect| rect.size()) {
                if size.x > 0.0 && size.y > 0.0 {
                    self.user_preferences.mapping_editor_width = size.x;
                    self.user_preferences.mapping_editor_height = size.y;
                }
            }
        }
        self.user_preferences.mapping_editor_maximized = maximized;

        let (zoom, pan, animations) = self.mapping_editor.view_preferences();
        self.user_preferences.mapping_editor_zoom = zoom;
        self.user_preferences.mapping_editor_viewport_x = pan.x;
        self.user_preferences.mapping_editor_viewport_y = pan.y;
        self.user_preferences.animations_enabled = animations;
    }

    /// Takes only what needs AutoCAD (the drawing's layers); standards, categories,
    /// and memory are loaded by this app itself.
    fn set_snapshot(&mut self, snapshot: DrawingSnapshot) {
        self.drawing_name = snapshot.drawing_name;
        self.drawing_id = snapshot.drawing_id;
        self.empty_layers = snapshot.empty_layers.into_iter().collect();
        let standard = std::mem::take(&mut self.standard_layers);
        self.set_layers(snapshot.source_layers, standard);
    }

    fn request_standard_if_needed(&mut self, ctx: &egui::Context) {
        if self.standard_requested || !self.standard_layers.is_empty() {
            return;
        }
        self.standard_requested = true;
        let path = self.plugin_config.template_dwg_path.clone();
        if path.is_empty() {
            return;
        }
        self.request_standard(ctx, path);
    }

    fn request_standard(&mut self, ctx: &egui::Context, path: String) {
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        self.apply_pending = true;
        self.status_message = "Loading standard drawing in AutoCAD…".to_string();
        std::thread::spawn(move || {
            let result = fetch_standard(path);
            let _ = sender.send(AppMessage::Ipc(result));
            repaint.request_repaint();
        });
    }

    /// Remembers the chosen standard for next launch, unless the config file was
    /// unreadable (saving would replace it with defaults).
    fn remember_template_path(&mut self, path: String) {
        self.plugin_config.template_dwg_path = path.clone();
        if !self.config_writable {
            return;
        }
        if let Some(config_path) = &self.config_path {
            if let Err(error) = PluginConfig::set_template_path(config_path, &path) {
                self.error_message =
                    Some(format!("Could not remember the chosen standard: {error}"));
            }
        }
    }

    /// The Choose Standard flow (right panel, Target header and Settings panel).
    fn choose_standard_file(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let selected = frame.winit_window().and_then(|window| {
            rfd::FileDialog::new()
                .set_title("Choose Standard Drawing")
                .add_filter("AutoCAD drawings", &STANDARD_FILE_EXTENSIONS)
                .set_parent(window)
                .pick_file()
        });
        if let Some(path) = selected {
            self.request_standard(ctx, path.to_string_lossy().into_owned());
        }
    }
}

/// The Settings panel and the beta notice banner (wording lives in `settings_panel`).
impl LayerStandardizerApp {
    /// Checks whether the Standards File exists on a worker thread (it is often on
    /// a slow network drive); the panel says "Checking..." until the answer arrives.
    fn check_standard_file(&mut self, ctx: &egui::Context) {
        self.standard_file_exists = None;
        let path = self.plugin_config.template_dwg_path.clone();
        if path.trim().is_empty() {
            return;
        }
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let exists = std::path::Path::new(&path).exists();
            let _ = sender.send(AppMessage::StandardExists { path, exists });
            repaint.request_repaint();
        });
    }

    fn show_notice_banner(&mut self, ctx: &egui::Context) {
        if !self.notice_visible {
            return;
        }
        egui::Area::new(egui::Id::new("first_run_notice"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 12.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(31, 33, 37, 244))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(235, 176, 20),
                    ))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_max_width(560.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(settings_panel::BETA_NOTICE)
                                        .color(egui::Color32::from_rgb(230, 230, 230)),
                                )
                                .wrap(),
                            );
                            if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                                self.notice_visible = false;
                            }
                        });
                    });
            });
    }

    /// Draws the Settings panel when open; returns the action the user picked.
    fn show_settings_panel(
        &mut self,
        ctx: &egui::Context,
    ) -> Option<settings_panel::SettingsAction> {
        use settings_panel::SettingsAction;
        if !self.settings.open {
            return None;
        }
        let template_path = self.plugin_config.template_dwg_path.as_str();
        let standard =
            settings_panel::standards_file_display(template_path, self.standard_file_exists);
        let memory_path = self
            .memory_store
            .as_ref()
            .map(|store| store.file_path().display().to_string());
        let enabled = settings_panel::settings_actions_enabled(self.apply_pending);
        let about = settings_panel::about_line(env!("CARGO_PKG_VERSION"));
        let status = self.settings.status.as_str();
        let mut open = true;
        let mut action = None;
        let heading = |ui: &mut egui::Ui, text: &str| {
            ui.label(
                egui::RichText::new(text)
                    .strong()
                    .size(15.0)
                    .color(egui::Color32::from_rgb(230, 230, 230)),
            );
            ui.add_space(6.0);
        };
        let divider = |ui: &mut egui::Ui| {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);
        };
        // Free-floating: no anchor, so the user can drag it anywhere and resize it.
        // default_pos only applies the first time the window is shown.
        let width = 440.0;
        let default_pos = ctx.content_rect().center() - egui::vec2(width / 2.0, 220.0);
        egui::Window::new("Settings")
            .id(egui::Id::new("settings_panel"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .movable(true)
            .order(egui::Order::Foreground)
            .default_pos(default_pos)
            .default_width(width)
            .min_width(360.0)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(18))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        heading(ui, "Standards File");
                        // Show the shortened path (C:\…\file.dws) while the file is known to
                        // exist; keep the status text for "Checking...", "Unavailable" and empty.
                        let shown = if standard.missing
                            || template_path.trim().is_empty()
                            || standard.text == "Checking..."
                        {
                            standard.text.clone()
                        } else {
                            settings_panel::shorten_path_for_display(template_path)
                        };
                        let text = if standard.missing {
                            egui::RichText::new(shown).color(egui::Color32::from_rgb(235, 176, 20))
                        } else {
                            egui::RichText::new(shown)
                        };
                        let label = ui.add(egui::Label::new(text).truncate());
                        if !template_path.trim().is_empty() {
                            label.on_hover_text(template_path);
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(enabled, egui::Button::new("Change..."))
                                .clicked()
                            {
                                action = Some(SettingsAction::ChangeStandard);
                            }
                        });

                        divider(ui);

                        heading(ui, "Memory File");
                        match &memory_path {
                            Some(path) => {
                                ui.add(egui::Label::new(path.as_str()).truncate())
                                    .on_hover_text(path.as_str());
                            }
                            None => {
                                ui.label(settings_panel::NO_MEMORY_LOCATION);
                            }
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(enabled, |ui| {
                                if ui.button("Change...").clicked() {
                                    action = Some(SettingsAction::ChangeMemoryFile);
                                }
                                if ui.button("Import...").clicked() {
                                    action = Some(SettingsAction::ImportMemory);
                                }
                                if ui.button("Export...").clicked() {
                                    action = Some(SettingsAction::ExportMemory);
                                }
                            });
                        });
                        if !status.is_empty() {
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(status)
                                        .color(egui::Color32::from_rgb(190, 190, 190)),
                                )
                                .wrap(),
                            );
                        }

                        divider(ui);

                        heading(ui, "About");
                        ui.label(egui::RichText::new(about.as_str()).size(13.0));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            // Opens in the default browser; the app itself makes no network calls.
                            ui.hyperlink_to("Project website", settings_panel::PROJECT_WEBSITE_URL);
                            ui.label("·");
                            ui.hyperlink_to("GitHub", settings_panel::PROJECT_REPO_URL);
                        });
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(settings_panel::BETA_NOTICE)
                                    .color(egui::Color32::from_rgb(190, 190, 190)),
                            )
                            .wrap(),
                        );
                    });
            });
        self.settings.open = open;
        action
    }

    fn handle_settings_action(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        action: settings_panel::SettingsAction,
    ) {
        use settings_panel::SettingsAction;
        if !settings_panel::settings_actions_enabled(self.apply_pending) {
            return;
        }
        match action {
            SettingsAction::ChangeStandard => self.choose_standard_file(ctx, frame),
            SettingsAction::ChangeMemoryFile => self.change_memory_file(frame),
            SettingsAction::ImportMemory => self.import_memory(frame),
            SettingsAction::ExportMemory => self.export_memory(frame),
        }
    }

    fn change_memory_file(&mut self, frame: &eframe::Frame) {
        let (Some(config_path), Some(config_dir)) = (
            self.config_path.clone(),
            self.config_path
                .as_deref()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_path_buf),
        ) else {
            self.settings.status =
                "No settings folder is available, so the memory file cannot be changed."
                    .to_string();
            return;
        };
        let selected = frame.winit_window().and_then(|window| {
            rfd::FileDialog::new()
                .set_title("Choose Memory File")
                .add_filter("JSON files", &["json"])
                .set_file_name("standards_memory.json")
                .set_parent(window)
                .save_file()
        });
        let Some(path) = selected else {
            return;
        };
        match data::switch_memory_file(&config_path, &config_dir, &path.to_string_lossy()) {
            Ok(change) => {
                self.settings.status =
                    settings_panel::memory_changed_status(change.store.file_path());
                self.plugin_config = change.config;
                self.memory_store = Some(change.store);
                self.memory_mappings = change.memory.mappings;
                self.recalculate();
            }
            Err(message) => self.settings.status = message,
        }
    }

    fn import_memory(&mut self, frame: &eframe::Frame) {
        let Some(store) = &self.memory_store else {
            self.settings.status = settings_panel::NO_MEMORY_LOCATION.to_string();
            return;
        };
        let selected = frame.winit_window().and_then(|window| {
            rfd::FileDialog::new()
                .set_title("Import Memory")
                .add_filter("JSON files", &["json"])
                .set_parent(window)
                .pick_file()
        });
        let Some(path) = selected else {
            return;
        };
        let report = match store.import_from(&path) {
            Ok(report) => report,
            Err(error) => {
                self.settings.status = format!("Import failed: {error}.");
                return;
            }
        };
        match store.load_checked() {
            Ok(memory) => {
                self.memory_mappings = memory.mappings;
                self.recalculate();
                self.settings.status = settings_panel::import_status(&report);
            }
            Err(error) => {
                self.settings.status = format!(
                    "{} But the memory could not be reloaded: {error}.",
                    settings_panel::import_status(&report)
                );
            }
        }
    }

    fn export_memory(&mut self, frame: &eframe::Frame) {
        let Some(store) = &self.memory_store else {
            self.settings.status = settings_panel::NO_MEMORY_LOCATION.to_string();
            return;
        };
        let selected = frame.winit_window().and_then(|window| {
            rfd::FileDialog::new()
                .set_title("Export Memory")
                .add_filter("JSON files", &["json"])
                .set_file_name("standards_memory.json")
                .set_parent(window)
                .save_file()
        });
        let Some(path) = selected else {
            return;
        };
        self.settings.status = match store.export_to(&path) {
            Ok(()) => settings_panel::export_status(&path),
            Err(error) => format!("Export failed: {error}."),
        };
    }
}

/// Live sync: following the active AutoCAD drawing (decisions live in `live_sync`).
impl LayerStandardizerApp {
    fn current_drawing(&self) -> Option<&str> {
        (!self.drawing_id.is_empty()).then_some(self.drawing_id.as_str())
    }

    fn connected_status(&self) -> String {
        if self.no_drawing {
            return "No drawing is open".to_string();
        }
        let mut status = format!(
            "Connected to {} • {} drawing layers",
            self.drawing_name,
            self.source_layers.len()
        );
        if !self.standard_layers.is_empty() {
            status.push_str(&format!(
                " • {} standard layers",
                self.standard_layers.len()
            ));
        }
        status
    }

    fn start_feed(&mut self, ctx: &egui::Context) {
        if self.feed.is_some() {
            return;
        }
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        self.feed = Some(acad_layer_ipc::spawn_feed(
            self.pending_report.clone(),
            move |message| {
                // A poll in flight when the window closed may still deliver; the
                // receiver is gone by then, so the send error is ignored.
                let _ = sender.send(AppMessage::Feed(message));
                repaint.request_repaint();
            },
        ));
    }

    /// Runs every frame: pause state, deferred layer reads, and the pending report.
    fn drive_live_sync(&mut self, ctx: &egui::Context) {
        let report = if self.close_approved {
            // The window is closing: nothing it holds can be applied any more.
            Vec::new()
        } else {
            self.sessions.pending_report(
                self.current_drawing()
                    .map(|id| (id, self.mapping_editor.unapplied_count())),
            )
        };
        if let Some(feed) = &self.feed {
            // Never paused while connections are pending: the check-ins are what
            // keeps AutoCAD from closing a drawing that has them.
            feed.set_paused(live_sync::feed_should_pause(
                ctx.input(|input| input.viewport().minimized),
                !report.is_empty(),
            ));
            let now = Instant::now();
            let busy = self.apply_pending || self.connection_lost;
            if let Some(read) = self.reads.next(busy, now) {
                self.start_layer_read(ctx, read);
            }
            if let Some(delay) = self.reads.retry_in(now) {
                ctx.request_repaint_after(delay);
            }
        }
        *self
            .pending_report
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = report;
    }

    fn start_layer_read(&self, ctx: &egui::Context, read: LayerRead) {
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = match read {
                LayerRead::Drawing(id) => acad_layer_ipc::get_layers_for_drawing(id),
                LayerRead::Active => read_active_drawing_layers(),
            };
            let _ = sender.send(AppMessage::Layers(result));
            repaint.request_repaint();
        });
    }

    fn on_feed_message(&mut self, ctx: &egui::Context, message: FeedMessage) {
        let read = live_sync::feed_status_read(&message);
        match message {
            FeedMessage::Events(events) => {
                let current = self.reads.showing_next(&self.drawing_id).to_string();
                let plan = live_sync::plan_feed_events(&current, &events);
                for id in &plan.forget {
                    self.sessions.forget(id);
                    self.reads.cancel_drawing(id);
                    self.close_dialogs.forget_drawing(id);
                    if *id == self.drawing_id {
                        // The drawing closed, so its connections go with it.
                        drop(self.mapping_editor.take_edit_state());
                        self.show_no_drawing();
                    }
                }
                for (kind, id) in &plan.blocked {
                    if !plan.forget.contains(id) {
                        self.on_close_blocked(ctx, kind, id);
                    }
                }
                if let Some(id) = plan.switch_to {
                    self.reads.want(LayerRead::Drawing(id));
                }
                if plan.refresh_current {
                    self.reads.want_refresh(&current);
                }
            }
            FeedMessage::Resync => {}
            FeedMessage::Down(_) => {
                self.connection_lost = true;
                self.status_message = "AutoCAD connection lost".to_string();
            }
            FeedMessage::Up => {
                self.connection_lost = false;
                self.status_message = self.connected_status();
            }
        }
        if let Some(read) = read {
            self.reads.want(read);
        }
    }

    fn on_layer_read(&mut self, result: Result<IpcResponse, String>) {
        let ok = matches!(
            result,
            Ok(IpcResponse::DrawingLayers(_)) | Ok(IpcResponse::NoActiveDrawing)
        );
        let current = self.reads.finished(ok, Instant::now());
        match result {
            Ok(IpcResponse::DrawingLayers(info)) if current => self.show_drawing_layers(info),
            Ok(IpcResponse::NoActiveDrawing) if current => {
                // No drawing is active, but the displayed one may still be open: keep
                // its connections; a DrawingClosed event discards them if it closed.
                if let Some(id) = self.current_drawing().map(str::to_string) {
                    let state = self.mapping_editor.take_edit_state();
                    self.sessions.stash(&id, &self.drawing_name, state);
                }
                self.show_no_drawing();
            }
            Ok(IpcResponse::DrawingLayers(_)) | Ok(IpcResponse::NoActiveDrawing) => {}
            // Never blank the Source side on a failed read (it may be transient):
            // report it and retry shortly.
            Ok(IpcResponse::Error(message)) | Err(message) => {
                if !self.connection_lost {
                    self.status_message = message;
                }
            }
            Ok(_) => {
                if !self.connection_lost {
                    self.status_message =
                        "AutoCAD returned an unexpected IPC response.".to_string();
                }
            }
        }
    }

    /// Clears the Source side. The caller has already stashed or dropped the edits.
    fn show_no_drawing(&mut self) {
        self.mapping_editor.clear_selection();
        self.drawing_id.clear();
        self.drawing_name.clear();
        self.source_layers.clear();
        self.empty_layers.clear();
        self.no_drawing = true;
        self.recalculate();
        self.status_message = "No drawing is open".to_string();
    }

    fn show_drawing_layers(&mut self, info: DrawingLayersInfo) {
        let switching = live_sync::is_switch(&self.drawing_id, self.no_drawing, &info.drawing_id);
        let changed = !switching
            && live_sync::layers_differ(
                &self.source_layers,
                &self.empty_layers,
                &info.source_layers,
                &info.empty_layers,
            );
        if switching {
            let previous = self.mapping_editor.take_edit_state();
            if let Some(id) = self.current_drawing().map(str::to_string) {
                self.sessions.stash(&id, &self.drawing_name, previous);
            }
            self.mapping_editor.clear_selection();
            let mut state = self.sessions.take(&info.drawing_id);
            sessions::drop_missing_sources(&mut state.overrides, &info.source_layers);
            // The standard may have changed since this state was stashed.
            sessions::retain_valid_targets(&mut state, &self.standard_layers);
            self.mapping_editor.restore_edit_state(state);
        } else {
            sessions::drop_missing_sources(&mut self.mapping_editor.overrides, &info.source_layers);
        }
        let was_blank = self.no_drawing;
        self.drawing_id = info.drawing_id;
        self.drawing_name = info.drawing_name;
        self.empty_layers = info.empty_layers.into_iter().collect();
        self.source_layers = info.source_layers;
        self.no_drawing = false;
        self.recalculate();
        if switching || was_blank {
            self.status_message = self.connected_status();
        } else if changed {
            self.status_message = "Layers changed in AutoCAD — list refreshed".to_string();
        }
    }
}

/// Close protection: the Apply / Discard / Cancel dialog (decisions live in
/// `close_dialog`).
impl LayerStandardizerApp {
    /// Plain Apply of the displayed drawing (the editor's Apply, or the dialog's).
    fn start_apply(&mut self, ctx: &egui::Context, remember: bool) {
        let mappings = self.mapping_editor.current_mappings(&self.matches);
        // The Rust app owns translation memory: it writes it after AutoCAD confirms
        // the Apply, so the connector is always told not to.
        self.pending_remember = remember.then(|| {
            (
                self.source_layers.clone(),
                mappings
                    .iter()
                    .map(|m| (m.source_layer.clone(), m.target_layer.clone()))
                    .collect(),
            )
        });
        let request = (
            self.drawing_name.clone(),
            self.drawing_id.clone(),
            mappings,
            false,
            self.mapping_editor.property_settings(),
        );
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        self.apply_pending = true;
        self.status_message = "Applying mappings in AutoCAD…".to_string();
        std::thread::spawn(move || {
            let result =
                acad_layer_ipc::apply_plan(request.0, request.1, request.2, request.3, request.4);
            let _ = sender.send(AppMessage::Ipc(result));
            repaint.request_repaint();
        });
    }

    /// Closes the window without asking again.
    fn close_window(&mut self, ctx: &egui::Context) {
        self.close_approved = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The drawing the dialog is about: its name and unapplied count.
    fn close_target(&self, reason: &close_dialog::CloseReason) -> (String, usize) {
        let displayed = (
            self.drawing_name.clone(),
            self.mapping_editor.unapplied_count(),
        );
        match reason {
            close_dialog::CloseReason::DrawingClose(id) if *id != self.drawing_id => {
                self.sessions.pending_of(id).unwrap_or_default()
            }
            _ => displayed,
        }
    }

    /// Every connection the close would lose (for a quit or the window, all drawings').
    fn close_count(&self, reason: &close_dialog::CloseReason) -> usize {
        let (_, count) = self.close_target(reason);
        match reason {
            close_dialog::CloseReason::Quit | close_dialog::CloseReason::WindowClose => {
                count
                    + self
                        .sessions
                        .others_pending(self.current_drawing())
                        .iter()
                        .map(|(_, count)| count)
                        .sum::<usize>()
            }
            _ => count,
        }
    }

    /// AutoCAD blocked a close or quit because this window reported unapplied
    /// connections: ask the user now, in front even if minimized (a direct answer to
    /// their own close; the only time this window takes focus by itself).
    fn on_close_blocked(&mut self, ctx: &egui::Context, kind: &str, drawing_id: &str) {
        let Some(reason) = close_dialog::blocked_reason(kind, drawing_id) else {
            return;
        };
        if self.close_count(&reason) == 0 {
            // Nothing is pending any more (the block came from an older report):
            // let the user's close go ahead.
            self.replay_close(ctx, &reason);
            return;
        }
        self.close_dialogs.offer(reason);
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Drops the connections the close would lose: that drawing's for a drawing
    /// close, every drawing's for a quit or the window closing.
    fn discard_for_close(&mut self, reason: &close_dialog::CloseReason) {
        match reason {
            close_dialog::CloseReason::DrawingClose(id) => {
                if *id == self.drawing_id {
                    drop(self.mapping_editor.take_edit_state());
                }
                self.sessions.forget(id);
            }
            close_dialog::CloseReason::WindowClose | close_dialog::CloseReason::Quit => {
                drop(self.mapping_editor.take_edit_state());
                self.sessions.forget_all();
            }
        }
    }

    /// Asks the connector to re-run the blocked close or quit. The pending report is
    /// zeroed for it first, under the lock, so a poll cannot restore the old count
    /// before the replayed command runs; the request itself runs off the UI thread.
    fn replay_close(&mut self, ctx: &egui::Context, reason: &close_dialog::CloseReason) {
        let Some((kind, drawing_id)) = close_dialog::replay_target(reason, &self.drawing_id) else {
            return;
        };
        let report = close_dialog::pending_after_resolving(
            self.sessions.pending_report(
                self.current_drawing()
                    .map(|id| (id, self.mapping_editor.unapplied_count())),
            ),
            reason,
        );
        *self
            .pending_report
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = report.clone();
        let sender = self.ipc_sender.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = acad_layer_ipc::replay_close(kind, drawing_id, report);
            let _ = sender.send(AppMessage::Replay(result));
            repaint.request_repaint();
        });
    }

    fn on_replay(&mut self, ctx: &egui::Context, result: Result<IpcResponse, String>) {
        let message = match result {
            Ok(IpcResponse::Replayed) => {
                if std::mem::take(&mut self.close_after_replay) {
                    self.close_window(ctx);
                }
                return;
            }
            Ok(IpcResponse::Error(message)) | Err(message) => message,
            Ok(_) => "AutoCAD returned an unexpected IPC response.".to_string(),
        };
        // Never swallow a failed replay: show it and keep the window open.
        self.close_after_replay = false;
        self.status_message = message.clone();
        self.error_message = Some(match self.error_message.take() {
            Some(earlier) => format!("{earlier}\n\n{message}"),
            None => message,
        });
    }

    /// Draws the shown close dialog, if any, and acts on the user's choice.
    fn show_close_dialog(&mut self, ctx: &egui::Context) {
        let Some(reason) = self.close_dialogs.current().cloned() else {
            return;
        };
        if self.close_count(&reason) == 0 {
            // Applied or removed while the dialog was up: nothing left to ask about.
            self.close_dialogs.resolve();
            return;
        }
        let (name, count) = self.close_target(&reason);
        // All other pending drawings; `dialog_message` decides which are at risk.
        let others = self.sessions.others_pending(self.current_drawing());
        let shown = if self.no_drawing || self.reads.switch_pending(&self.drawing_id) {
            ""
        } else {
            self.drawing_id.as_str()
        };
        let apply_offered = close_dialog::can_apply(&reason, shown, count);
        let text = close_dialog::dialog_message(&reason, &name, count, &others, apply_offered);
        let mut choice = None;
        egui::Window::new("Layer Herder")
            .id(egui::Id::new("close_dialog"))
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(text);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.apply_pending, |ui| {
                        if apply_offered && ui.button("Apply").clicked() {
                            choice = Some(close_dialog::CloseChoice::Apply);
                        }
                        if ui.button("Discard").clicked() {
                            choice = Some(close_dialog::CloseChoice::Discard);
                        }
                    });
                    if ui.button("Cancel").clicked() {
                        choice = Some(close_dialog::CloseChoice::Cancel);
                    }
                });
            });
        if let Some(choice) = choice {
            self.on_close_choice(ctx, reason, choice);
        }
    }

    fn on_close_choice(
        &mut self,
        ctx: &egui::Context,
        reason: close_dialog::CloseReason,
        choice: close_dialog::CloseChoice,
    ) {
        match choice {
            close_dialog::CloseChoice::Cancel => self.close_dialogs.resolve(),
            close_dialog::CloseChoice::Discard => {
                self.close_dialogs.resolve();
                self.discard_for_close(&reason);
                if reason == close_dialog::CloseReason::WindowClose {
                    self.close_window(ctx);
                } else {
                    self.status_message = "Unapplied connections discarded.".to_string();
                    self.replay_close(ctx, &reason);
                }
            }
            close_dialog::CloseChoice::Apply => {
                if let Some(message) = live_sync::blocked_action_message(
                    self.no_drawing,
                    self.reads.switch_pending(&self.drawing_id),
                ) {
                    // The dialog stays and is shown again once the error is dismissed.
                    self.status_message = message.to_string();
                    self.error_message = Some(message.to_string());
                    return;
                }
                self.close_dialogs.resolve();
                self.apply_for_close = Some(reason);
                self.start_apply(ctx, false);
            }
        }
    }
}

impl Drop for LayerStandardizerApp {
    fn drop(&mut self) {
        if let Some(feed) = &mut self.feed {
            if self.connection_lost {
                // AutoCAD is unreachable: no last report to send, so do not wait.
                feed.stop();
            } else {
                // Let the feed's last, empty report reach the connector, so a drawing
                // close right after the window is gone is not refused; never hold up
                // the exit for long.
                feed.stop_and_wait(FEED_EXIT_WAIT);
            }
        }
    }
}

impl LayerStandardizerApp {
    fn attach_to_owner_if_requested(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        if self.owner_attached || self.owner_hwnd.is_none() {
            return;
        }

        let result = attach_native_window_to_owner(frame, self.owner_hwnd.unwrap());
        self.owner_attached = true;
        if let Err(message) = result {
            self.status_message = format!("Could not attach window to AutoCAD: {message}");
        }

        // The root window starts hidden when an owner was requested, so it
        // cannot flash as an unrelated top-level window before attachment.
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }
}

/// Makes the window owned by AutoCAD's main window. The Win32 call lives in the
/// shared kit; this only gets the window's handle from eframe.
fn attach_native_window_to_owner(frame: &eframe::Frame, owner_hwnd: isize) -> Result<(), String> {
    let window = frame
        .winit_window()
        .ok_or_else(|| "the native window handle is unavailable".to_string())?;
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("the UI window does not have a Win32 handle".to_string());
    };
    acad_bridge::owner::attach_to_owner(handle.hwnd.get(), owner_hwnd)
}

/// Title-bar and taskbar icon. Raw 64x64 RGBA, generated with the other
/// brand sizes by scripts/generate-brand-assets.py, so no image decoder is
/// needed at runtime. The exe file's own icon is embedded by build.rs.
fn app_icon() -> egui::IconData {
    egui::IconData {
        rgba: include_bytes!("../assets/app_icon_64.rgba").to_vec(),
        width: 64,
        height: 64,
    }
}

fn main() -> eframe::Result<()> {
    let args = acad_bridge::parse_launch_args(std::env::args().skip(1));
    let owner_hwnd = args.owner_hwnd;
    let first_run_notice = args.has_flag("--first-run-notice");

    let user_preferences = load_preferences();
    let storage_path = preferences_path().map(|path| path.with_file_name("rust_ui_state.ron"));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([
                user_preferences.mapping_editor_width,
                user_preferences.mapping_editor_height,
            ])
            .with_maximized(user_preferences.mapping_editor_maximized)
            .with_title("Layer Herder")
            .with_icon(app_icon())
            .with_visible(owner_hwnd.is_none()),
        persistence_path: storage_path,
        persist_window: false,
        ..Default::default()
    };

    let (ipc_tx, ipc_results) = std::sync::mpsc::channel();

    eframe::run_native(
        "Layer Herder",
        options,
        Box::new(move |cc| {
            if owner_hwnd.is_some() {
                let snapshot_sender = ipc_tx.clone();
                let repaint = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    let _ = snapshot_sender
                        .send(AppMessage::Ipc(acad_layer_ipc::request_drawing_snapshot()));
                    repaint.request_repaint();
                });
            }
            Ok(Box::new(LayerStandardizerApp::new(
                owner_hwnd,
                ipc_tx,
                ipc_results,
                user_preferences,
                first_run_notice,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_matching_ignores_case_for_non_ascii_names_like_csharp() {
        let standard = vec!["É-WALL".to_string(), "A-DOOR".to_string()];
        let source = vec!["é-sourcé".to_string()];
        let memory = HashMap::from([("É-SOURCÉ".to_string(), "é-wall".to_string())]);
        let matches = compute_matches(&source, &standard, &memory, 0.6);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].target_layer.as_deref(), Some("É-WALL"));
        assert_eq!(matches[0].source, MatchSource::Memory);
    }

    #[test]
    fn standard_picker_accepts_dws_templates() {
        assert!(STANDARD_FILE_EXTENSIONS.contains(&"dws"));
        assert!(STANDARD_FILE_EXTENSIONS.contains(&"dwg"));
    }

    #[test]
    fn fetch_standard_reports_a_missing_file_without_asking_autocad() {
        let error = fetch_standard("Z:/definitely/not/here.dws".to_string()).unwrap_err();
        assert!(error.contains("unavailable"), "{error}");
        assert!(error.contains("here.dws"), "{error}");
    }
}
