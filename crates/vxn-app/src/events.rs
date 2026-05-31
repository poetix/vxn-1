//! UI / host / view event enums (ADR 0007 §3).
//!
//! Channels carry these between threads. `UiEvent` flows UI → controller,
//! `HostEvent` flows host shell → controller, `ViewEvent` flows controller →
//! UI. The controller is the only writer of the model.

use std::path::PathBuf;

use crate::domain::{KeyMode, Layer, PresetMeta};
use crate::model::ParamId;

/// Where a preset is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetSource {
    /// Index into the embedded factory bank (`vxn_engine::factory()`).
    Factory { index: usize },
    /// Absolute path under the user preset directory.
    User { path: PathBuf },
}

/// Intent posted by the editor to the controller.
#[derive(Clone, Debug)]
pub enum UiEvent {
    SetParam { id: ParamId, plain: f32 },
    SetParamNorm { id: ParamId, norm: f32 },
    BeginGesture { id: ParamId },
    EndGesture { id: ParamId },
    /// Reset every per-patch param of `layer` to its descriptor default. Each
    /// write is gesture-bracketed so the host records the jump as one edit.
    /// Globals and the other layer are left untouched.
    ResetLayer { layer: Layer },
    LoadPreset { source: PresetSource },
    /// Walk the combined Factory + User preset list by `delta` steps,
    /// wrapping at either end, and load the resulting entry (0049).
    /// Controller-side: the editor doesn't need to track the index — it
    /// posts `delta = ±1` for prev/next and the controller resolves
    /// against the same ordered list it publishes for the browser.
    StepPreset { delta: i32 },
    SavePreset { name: String, folder: Option<String> },
    RenamePreset { path: PathBuf, new_name: String },
    DeletePreset { path: PathBuf },
    MovePreset { path: PathBuf, dest_folder: Option<String> },
    RenameFolder { old_name: String, new_name: String },
    DeleteFolder { name: String },
    NewFolder { suggested: String },
    SetKeyMode { mode: KeyMode },
    SetSplitPoint { note: u8 },
    SetEditLayer { layer: Layer },
    /// Editor has finished its initial JS init and is ready to receive
    /// view events. Triggers a full re-broadcast of every param, the key
    /// mode, etc. so the page is correctly seeded even when the very
    /// first timer-driven push raced ahead of the inline bootstrap script.
    /// The vizia editor never sends this — its on_idle hook polls
    /// `SharedParams` directly and doesn't need a re-broadcast.
    EditorReady,
    /// Faceplate asks the editor backend to pop a floating text-input
    /// window (host kbd capture workaround, 0048 / E011). `id` is a
    /// JS-chosen correlation token returned verbatim in the matching
    /// [`UiEvent::TextInputResult`]; `title` is the popup title;
    /// `initial` seeds the text field. Controller just relays — the
    /// backend (vxn-ui-web on macOS) opens the NSWindow.
    RequestTextInput {
        id: String,
        title: String,
        initial: String,
    },
    /// Floating text-input popup committed (`Some`) or cancelled
    /// (`None`). Posted by the editor backend; controller forwards as
    /// [`ViewEvent::TextInputResult`] so the originating JS callback
    /// can fire.
    TextInputResult {
        id: String,
        value: Option<String>,
    },
}

/// Event extracted from the host's CLAP stream and handed to the controller.
///
/// `StateLoaded` carries the raw blob the host gave us; the model deserializes
/// it via [`ParamModel::restore_from_bytes`]. Keeping the blob opaque here lets
/// `vxn-app` stay engine-free.
#[derive(Clone, Debug)]
pub enum HostEvent {
    ParamAutomation { id: ParamId, plain: f32 },
    StateLoaded { blob: Vec<u8> },
    Tempo { bpm: f32 },
}

/// View-bound update the controller emits. The editor drains these on idle
/// and reseeds its widget signals; no other path mutates the view's data.
#[derive(Clone, Debug)]
pub enum ViewEvent {
    ParamChanged {
        id: ParamId,
        plain: f32,
        norm: f32,
        display: String,
    },
    PresetLoaded {
        meta: PresetMeta,
        source: Option<PresetSource>,
        warnings: Vec<String>,
    },
    /// The user-preset corpus on disk changed (save / rename / delete /
    /// move / new folder). The editor re-reads the snapshot the controller
    /// publishes via `CorpusHandle`. `follow` carries the on-disk path of the
    /// preset that triggered the change (e.g. just-saved / just-renamed /
    /// just-moved), so the view can move its cursor onto that entry; `None`
    /// for changes with no single follow target (delete, new folder).
    PresetCorpusChanged {
        follow: Option<PathBuf>,
    },
    KeyModeChanged {
        mode: KeyMode,
    },
    /// The view's edit-layer selection just changed (Upper ↔ Lower). Pure
    /// view state — emitted in response to [`UiEvent::SetEditLayer`] so
    /// editors that don't own the layer-toggle widget (e.g. the HTML
    /// faceplate when its layer flipper sits elsewhere) can still rebind
    /// per-patch panels to the new layer's CLAP ids. The vizia editor
    /// tracks its own `edit_layer` signal locally and ignores this.
    EditLayerChanged {
        layer: Layer,
    },
    Status {
        line: String,
    },
    /// Backend-bound: open the floating text-input popup (0048). Not
    /// forwarded to the page — the editor backend intercepts in its
    /// `push_view_event` impl and pops a native window. `id` matches
    /// the originating [`UiEvent::RequestTextInput`].
    OpenTextInput {
        id: String,
        title: String,
        initial: String,
    },
    /// Page-bound result of a text-input popup. The JS dispatcher
    /// fires the pending callback keyed by `id`. `value` is `None` on
    /// cancel (Esc, click outside) or `Some` on commit.
    TextInputResult {
        id: String,
        value: Option<String>,
    },
}
