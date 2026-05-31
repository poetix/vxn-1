//! Controller (ADR 0007 §1, §3).
//!
//! Owns the inbound UI + host queues and the outbound view queue. `tick()` is
//! the sole place model mutation happens off the audio thread.

use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use crate::domain::{Layer, PresetMeta};
use crate::events::{HostEvent, PresetSource, UiEvent, ViewEvent};
use crate::model::{ParamId, ParamModel};
use crate::params::{PATCH_COUNT, PatchParam, desc_for_clap_id, patch_clap_id};
use crate::preset::{PresetCorpus, PresetStore};

/// Shared snapshot of the preset corpus the controller publishes for the view.
/// Read on idle when the controller emits [`ViewEvent::PresetCorpusChanged`];
/// re-seeded by the controller after every save / rename / delete / move /
/// new-folder op.
pub type CorpusHandle = Arc<Mutex<PresetCorpus>>;

/// Main-thread closure that drains both inbound queues and applies their
/// effects to the model — i.e. [`Controller::tick`] wrapped behind a type-
/// erased boundary. The clack shell builds it from its `Arc<Mutex<Controller>>`
/// so the editor crate can pump the controller on idle without depending on
/// the concrete `Controller<M>` type.
pub type Tick = Arc<dyn Fn() + Send + Sync + 'static>;

/// Bounded-channel depth. Sized for a preset-load burst (one ParamChanged per
/// CLAP id, ~hundreds) with headroom. Revisit if it ever saturates (ADR 0007
/// open question §3).
pub const CHANNEL_CAPACITY: usize = 1024;

/// Cheap-clone post handle for the UI side. Holds only the UI sender; the
/// host sender lives in the clack shell.
pub struct ControllerHandle {
    ui: SyncSender<UiEvent>,
}

impl Clone for ControllerHandle {
    fn clone(&self) -> Self {
        Self { ui: self.ui.clone() }
    }
}

impl ControllerHandle {
    /// Post a UI intent. Returns `Err` if the channel is full or the
    /// controller has been dropped.
    #[inline]
    pub fn post(&self, event: UiEvent) -> Result<(), TrySendError<UiEvent>> {
        self.ui.try_send(event)
    }
}

pub struct Controller<M: ParamModel> {
    model: Arc<M>,
    presets: Box<dyn PresetStore>,
    corpus: CorpusHandle,
    /// Last successfully loaded preset's source. Anchors the prev/next
    /// walker (0049): `UiEvent::StepPreset` resolves this against the
    /// combined ordered list and advances by `delta`.
    current_source: Option<PresetSource>,
    ui_tx: SyncSender<UiEvent>,
    ui_rx: Receiver<UiEvent>,
    host_tx: SyncSender<HostEvent>,
    host_rx: Receiver<HostEvent>,
    view_tx: SyncSender<ViewEvent>,
}

impl<M: ParamModel> Controller<M> {
    /// Build a controller bound to `model` and a preset store. Returns the
    /// controller, the receiver end of the view-event channel, and the shared
    /// corpus snapshot. Hand the receiver and the corpus handle to whoever
    /// opens the editor — the controller reseeds the corpus after every disk
    /// mutation and emits `PresetCorpusChanged` so the view re-reads.
    pub fn new(
        model: Arc<M>,
        presets: Box<dyn PresetStore>,
    ) -> (Self, Receiver<ViewEvent>, CorpusHandle) {
        let (ui_tx, ui_rx) = sync_channel(CHANNEL_CAPACITY);
        let (host_tx, host_rx) = sync_channel(CHANNEL_CAPACITY);
        let (view_tx, view_rx) = sync_channel(CHANNEL_CAPACITY);
        // Seed both corpus halves once. Factory is read-only; user is refreshed
        // on every disk-mutating UiEvent below.
        let factory: Vec<PresetMeta> = (0..presets.factory_len())
            .filter_map(|i| presets.factory_meta(i))
            .collect();
        let user = presets.list_user_tree();
        let corpus = Arc::new(Mutex::new(PresetCorpus { factory, user }));
        let ctrl = Self {
            model,
            presets,
            corpus: corpus.clone(),
            current_source: None,
            ui_tx,
            ui_rx,
            host_tx,
            host_rx,
            view_tx,
        };
        (ctrl, view_rx, corpus)
    }

    /// Re-read the user-side corpus from disk and refresh the shared snapshot.
    /// Factory entries are static; left alone. Called after every disk-mutating
    /// preset op so the view's `PresetCorpusChanged` drain finds fresh data.
    fn refresh_user_corpus(&self) {
        let user = self.presets.list_user_tree();
        if let Ok(mut c) = self.corpus.lock() {
            c.user = user;
        }
    }

    /// Cloneable post handle for the editor.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle { ui: self.ui_tx.clone() }
    }

    pub fn ui_sender(&self) -> SyncSender<UiEvent> {
        self.ui_tx.clone()
    }

    pub fn host_sender(&self) -> SyncSender<HostEvent> {
        self.host_tx.clone()
    }

    pub fn model(&self) -> &Arc<M> {
        &self.model
    }

    pub fn preset_store(&self) -> &dyn PresetStore {
        &*self.presets
    }

    /// Drain inbound queues and apply their effects.
    ///
    /// UI drains first so an in-flight gesture is bracketed correctly when
    /// host automation arrives in the same tick — host events landing during
    /// a gesture are folded into the model (the audio path needs them) but
    /// their view echo is suppressed until the gesture ends.
    pub fn tick(&mut self) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            self.handle_ui(ev);
        }
        while let Ok(ev) = self.host_rx.try_recv() {
            self.handle_host(ev);
        }
    }

    fn handle_ui(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::SetParam { id, plain } => {
                self.model.set(id, plain);
                self.emit_param_changed(id);
            }
            UiEvent::SetParamNorm { id, norm } => {
                self.model.set_normalized(id, norm);
                self.emit_param_changed(id);
            }
            UiEvent::BeginGesture { id } => {
                self.model.set_gesture(id, true);
            }
            UiEvent::EndGesture { id } => {
                self.model.set_gesture(id, false);
            }
            UiEvent::ResetLayer { layer } => {
                self.reset_layer(layer);
            }
            UiEvent::LoadPreset { source } => {
                self.load_preset(source);
            }
            UiEvent::StepPreset { delta } => {
                self.step_preset(delta);
            }
            UiEvent::SavePreset { name, folder } => {
                self.save_preset(name, folder);
            }
            UiEvent::RenamePreset { path, new_name } => {
                match self.presets.user_rename(&path, &new_name) {
                    Ok(new_path) => {
                        self.refresh_user_corpus();
                        self.send(ViewEvent::PresetCorpusChanged {
                            follow: Some(new_path),
                        });
                        self.send_status(format!("Renamed to {}", new_name.trim()));
                    }
                    Err(e) => self.send_status(format!("rename failed: {e}")),
                }
            }
            UiEvent::DeletePreset { path } => match self.presets.user_delete(&path) {
                Ok(()) => {
                    self.refresh_user_corpus();
                    self.send(ViewEvent::PresetCorpusChanged { follow: None });
                }
                Err(e) => self.send_status(format!("delete failed: {e}")),
            },
            UiEvent::MovePreset { path, dest_folder } => {
                match self.presets.user_move(&path, dest_folder.as_deref()) {
                    Ok(new_path) => {
                        self.refresh_user_corpus();
                        self.send(ViewEvent::PresetCorpusChanged {
                            follow: Some(new_path),
                        });
                    }
                    Err(e) => self.send_status(format!("move failed: {e}")),
                }
            }
            UiEvent::RenameFolder { old_name, new_name } => {
                match self.presets.user_rename_folder(&old_name, &new_name) {
                    Ok((_path, final_name)) => {
                        self.refresh_user_corpus();
                        self.send(ViewEvent::PresetCorpusChanged { follow: None });
                        self.send_status(format!("Renamed folder to {final_name}"));
                    }
                    Err(e) => self.send_status(format!("rename folder failed: {e}")),
                }
            }
            UiEvent::DeleteFolder { name } => {
                match self.presets.user_delete_folder(&name) {
                    Ok(()) => {
                        self.refresh_user_corpus();
                        self.send(ViewEvent::PresetCorpusChanged { follow: None });
                        self.send_status(format!("Deleted folder {name}"));
                    }
                    Err(e) => self.send_status(format!("delete folder failed: {e}")),
                }
            }
            UiEvent::NewFolder { suggested } => {
                match self.presets.user_create_folder(&suggested) {
                    Ok(_) => {
                        self.refresh_user_corpus();
                        self.send(ViewEvent::PresetCorpusChanged { follow: None });
                    }
                    Err(e) => self.send_status(format!("create folder failed: {e}")),
                }
            }
            UiEvent::SetKeyMode { mode } => {
                // Seeded variant: Whole → non-Whole copies Upper → Lower so
                // the lower layer starts equal to the upper before diverging.
                // State load uses plain `set_key_mode` (no seeding).
                self.model.set_key_mode_seeded(mode);
                self.send(ViewEvent::KeyModeChanged { mode });
                // Lower-layer params may have just been seeded from Upper —
                // republish them so the editor's signals follow.
                self.broadcast_all_params();
            }
            UiEvent::SetSplitPoint { note } => {
                self.model.set_split_point(note);
                self.send_status(format!("split point: {note}"));
            }
            UiEvent::SetEditLayer { layer } => {
                // No model mutation — the edit layer is pure view state.
                // Echo to the view so editors that don't own the toggle
                // widget (HTML faceplate) can rebind per-patch panels.
                self.send(ViewEvent::EditLayerChanged { layer });
            }
            UiEvent::RequestTextInput { id, title, initial } => {
                // Pure relay — controller doesn't peek at the value.
                // Backend (vxn-ui-web on macOS) intercepts the matching
                // ViewEvent in push_view_event and opens an NSWindow.
                self.send(ViewEvent::OpenTextInput { id, title, initial });
            }
            UiEvent::TextInputResult { id, value } => {
                // Forward to the page so its pending JS callback fires.
                self.send(ViewEvent::TextInputResult { id, value });
            }
            UiEvent::EditorReady => {
                // Editor has just finished its inline init and is now
                // listening on `onViewEvent`. Push the full param table +
                // key mode so any first-tick broadcast that landed before
                // the JS dispatcher was wired (a real race on slow page
                // loads — see vxn-ui-web's bootstrap notes) is replayed
                // into a known-ready listener.
                self.broadcast_all_params();
                self.send(ViewEvent::KeyModeChanged { mode: self.model.key_mode() });
                // 0050 race fix: the webview backend's first corpus push
                // can land before the page's bootstrap script has
                // installed `__vxn.applyPresetCorpus`. The webview's
                // flush_view_events keys its retry off
                // PresetCorpusChanged, so emit a benign one here to
                // force a corpus re-push after the page reports ready.
                self.send(ViewEvent::PresetCorpusChanged { follow: None });
            }
        }
    }

    fn handle_host(&mut self, ev: HostEvent) {
        match ev {
            HostEvent::ParamAutomation { id, plain } => {
                // Always write — the audio path must see the host value.
                self.model.set(id, plain);
                // Echo to view unless the user is actively dragging this
                // param: the editor's signal is the source of truth during
                // a gesture; host automation would yank the knob.
                if !self.model.gesture(id) {
                    self.emit_param_changed(id);
                }
            }
            HostEvent::StateLoaded { blob } => {
                if let Err(e) = self.model.restore_from_bytes(&blob) {
                    self.send_status(format!("state load failed: {e}"));
                    return;
                }
                self.send(ViewEvent::PresetLoaded {
                    meta: PresetMeta::default(),
                    source: None,
                    warnings: Vec::new(),
                });
                self.broadcast_all_params();
                self.send(ViewEvent::KeyModeChanged { mode: self.model.key_mode() });
            }
            HostEvent::Tempo { bpm: _ } => {
                // Routed through to the engine on a separate channel in a
                // future ticket; not stored in the model.
            }
        }
    }

    fn load_preset(&mut self, source: PresetSource) {
        let loaded = match &source {
            PresetSource::Factory { index } => self.presets.factory_load(*index),
            PresetSource::User { path } => self.presets.user_load(path),
        };
        match loaded {
            Ok(load) => {
                if let Err(e) = self.model.restore_from_bytes(&load.blob) {
                    self.send_status(format!("preset apply failed: {e}"));
                    return;
                }
                self.current_source = Some(source.clone());
                self.send(ViewEvent::PresetLoaded {
                    meta: load.meta,
                    source: Some(source),
                    warnings: load.warnings,
                });
                self.broadcast_all_params();
                self.send(ViewEvent::KeyModeChanged { mode: self.model.key_mode() });
            }
            Err(e) => self.send_status(format!("preset load failed: {e}")),
        }
    }

    /// Walk the combined Factory + User preset list by `delta` and load the
    /// resulting entry (0049 prev/next). Order: factory entries first
    /// (alpha-by-name), then user entries (alpha-by-name across folders);
    /// wraps at either end. With no prior preset, `delta >= 0` seeds at
    /// the first entry, `delta < 0` at the last — matches the vizia
    /// editor's `step_index` semantics so the walker behaves the same
    /// across backends.
    fn step_preset(&mut self, delta: i32) {
        let list = self.combined_preset_list();
        if list.is_empty() {
            return;
        }
        let cur_idx = self
            .current_source
            .as_ref()
            .and_then(|c| list.iter().position(|s| s == c));
        let len = list.len() as i32;
        let next = match cur_idx {
            Some(i) => (i as i32 + delta).rem_euclid(len) as usize,
            None if delta >= 0 => 0,
            None => (len - 1) as usize,
        };
        let source = list[next].clone();
        self.load_preset(source);
    }

    /// Combined factory + user preset list in walker order (0049).
    /// Factory entries first (alpha-by-name); then user entries across all
    /// folders (alpha-by-name). Built from the same corpus the browser
    /// reads so prev/next and the browser stay coherent.
    fn combined_preset_list(&self) -> Vec<PresetSource> {
        let corpus = match self.corpus.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut factory: Vec<(usize, &PresetMeta)> = corpus.factory.iter().enumerate().collect();
        factory.sort_by(|a, b| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()));
        let mut user: Vec<&crate::preset::UserPresetEntry> = corpus
            .user
            .iter()
            .flat_map(|f| f.presets.iter())
            .collect();
        user.sort_by(|a, b| a.meta.name.to_lowercase().cmp(&b.meta.name.to_lowercase()));
        let mut out: Vec<PresetSource> = Vec::with_capacity(factory.len() + user.len());
        for (i, _) in factory {
            out.push(PresetSource::Factory { index: i });
        }
        for p in user {
            out.push(PresetSource::User { path: p.path.clone() });
        }
        out
    }

    fn save_preset(&mut self, name: String, folder: Option<String>) {
        let blob = self.model.snapshot_bytes();
        let meta = PresetMeta {
            name: name.clone(),
            ..Default::default()
        };
        match self
            .presets
            .user_save(&name, folder.as_deref(), &meta, &blob)
        {
            Ok(path) => {
                self.refresh_user_corpus();
                self.send(ViewEvent::PresetCorpusChanged {
                    follow: Some(path.clone()),
                });
                self.send_status(format!("Saved {name}"));
            }
            Err(e) => self.send_status(format!("save failed: {e}")),
        }
    }

    /// Reset every per-patch param of `layer` to its descriptor default. Each
    /// write is bracketed by a gesture (like the UI's double-click reset) so
    /// the host echoes the jump as a recorded edit.
    fn reset_layer(&self, layer: Layer) {
        for p in 0..PATCH_COUNT {
            let pp = PatchParam::from_index(p).expect("PATCH_COUNT bound by enum");
            let raw = patch_clap_id(layer, pp);
            let id = ParamId::new(raw);
            let default = desc_for_clap_id(raw).map_or(0.0, |d| d.default);
            self.model.set_gesture(id, true);
            self.model.set(id, default);
            self.model.set_gesture(id, false);
            self.emit_param_changed(id);
        }
    }

    fn emit_param_changed(&self, id: ParamId) {
        let plain = self.model.get(id);
        let norm = self.model.get_normalized(id);
        let display = self
            .model
            .descriptor(id)
            .map(|d| d.display(plain))
            .unwrap_or_default();
        self.send(ViewEvent::ParamChanged {
            id,
            plain,
            norm,
            display,
        });
    }

    fn broadcast_all_params(&self) {
        let n = self.model.total();
        for i in 0..n {
            self.emit_param_changed(ParamId::new(i));
        }
    }

    fn send(&self, ev: ViewEvent) {
        // try_send drops on full — the view-event queue is sized for a
        // preset-load burst; a backed-up editor losing a redraw beat is
        // preferable to blocking the controller.
        let _ = self.view_tx.try_send(ev);
    }

    fn send_status(&self, line: String) {
        self.send(ViewEvent::Status { line });
    }
}
