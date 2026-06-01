//! VXN1 web editor backend (E010 / 0039 scaffold).
//!
//! A [`wry`] WebView attached as a child of the host's parent window. The HTML
//! is a placeholder for now — the real faceplate lands in 0040+. What ships
//! here is the *bridge*:
//!
//! - **JS → Rust:** the page calls `window.ipc.postMessage(json)`; the IPC
//!   handler parses one of the small set of opcodes below and posts the
//!   matching [`UiEvent`] onto the controller's UI sender.
//! - **Rust → JS:** [`EditorHandle::push_view_event`] serializes a
//!   [`ViewEvent`] and calls `webview.evaluate_script`, which the page picks
//!   up via `window.vxn.onViewEvent(ev)`. For 0039 the page just logs them;
//!   structured DOM updates land per-panel in 0041+.
//!
//! [`WebEditor`] is the [`EditorBackend`] impl the clack shell will hold once
//! 0047 flips it from vizia to this crate. Until then, the trait surface is
//! the contract a future shell programs against.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;

use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, WindowHandle as RwhWindowHandle,
};
use vxn_app::{
    ControllerHandle, CorpusHandle, EditorBackend, KeyMode, Layer, PATCH_COUNT, ParamDesc, ParamId,
    ParamKind, PresetCorpus, PresetSource, TOTAL_PARAMS, UNCATEGORIZED, UiEvent, ViewEvent,
    desc_for_clap_id,
};
use wry::{Rect, WebView, WebViewBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};

mod text_input;

/// Logical pixel dimensions of the editor. Matches the vizia editor's
/// [`vxn_ui_vizia::EDITOR_WIDTH`] / `_HEIGHT` so swapping backends doesn't reflow
/// the host's plugin window.
pub const EDITOR_WIDTH: u32 = 1024;
pub const EDITOR_HEIGHT: u32 = 772;

/// Max bytes per `evaluate_script` payload. The JSON-array literal interpolated
/// into the JS source is bounded here; under heavy automation (preset load
/// touches every param) the batch is split across multiple calls so wry never
/// sees a giant string. 100 KB is the ticket's "sane" cap.
const MAX_BATCH_BYTES: usize = 100_000;

/// Live editor. Dropping it tears down the WebView; on macOS wry removes the
/// subview from the parent NSView as part of that.
pub struct EditorHandle {
    webview: WebView,
    /// Per-tick batch buffer. The clack shell's `on_timer` calls
    /// [`Self::push_view_event`] once per event the controller produced this
    /// tick, then [`Self::flush_view_events`] once at the end — one
    /// `evaluate_script` per tick, not per event.
    buf: RefCell<Vec<ViewEvent>>,
    /// Raw native parent (NSView on macOS, HWND on Windows, xcb window id
    /// on Linux). Held for the editor's lifetime so the floating text-input
    /// popup (0048) can centre over the host plugin window without
    /// re-plumbing the parent through every ViewEvent.
    parent: *mut c_void,
    /// Controller post handle. The popup callback uses it to fire
    /// [`UiEvent::TextInputResult`] back when the user commits / cancels.
    ctrl: ControllerHandle,
    /// Shared preset corpus snapshot the controller refreshes on every
    /// disk-mutating preset op (0050). Serialized + pushed to JS at first
    /// flush and on every [`ViewEvent::PresetCorpusChanged`] in the batch
    /// so the browser panel stays in sync without a controller→view payload
    /// channel for the full corpus.
    corpus: CorpusHandle,
    /// `false` until the first batch flush has carried a corpus snapshot.
    /// On the next flush we always seed one so the page can render its
    /// browser even before any user-side mutation fires.
    corpus_seeded: Cell<bool>,
}

impl EditorHandle {
    /// Buffer one [`ViewEvent`] for the current tick. Flushed by
    /// [`Self::flush_view_events`]; nothing crosses into JS until then.
    ///
    /// [`ViewEvent::OpenTextInput`] is intercepted here and dispatched to
    /// the native popup primitive (0048) — it never reaches the JS bridge.
    /// On commit / cancel the popup posts [`UiEvent::TextInputResult`]
    /// through the controller, which echoes [`ViewEvent::TextInputResult`]
    /// back into this buffer for the page's pending-callback map.
    pub fn push_view_event(&self, event: ViewEvent) {
        if let ViewEvent::OpenTextInput { id, title, initial } = event {
            self.open_text_input(id, title, initial);
            return;
        }
        self.buf.borrow_mut().push(event);
    }

    fn open_text_input(&self, id: String, title: String, initial: String) {
        let ctrl = self.ctrl.clone();
        text_input::prompt_text(self.parent, &title, &initial, move |value| {
            // Channel-full / disconnected: nothing useful to do — the
            // popup is already torn down. Drop silently.
            let _ = ctrl.post(UiEvent::TextInputResult { id, value });
        });
    }

    /// Drain the batch into one `__vxn.applyViewEvents` call (or several, if
    /// the JSON exceeds [`MAX_BATCH_BYTES`]). `ParamChanged` events dedupe by
    /// id within the batch — only the latest value per param survives, which
    /// caps the bridge at one update per param per tick regardless of how
    /// many automation writes the audio thread did between ticks.
    ///
    /// Corpus seeding (0050): the preset corpus snapshot is sized like a
    /// few hundred metas and never deduped, so it ships as a separate
    /// `applyPresetCorpus` JS call rather than going through the
    /// [`ViewEvent`] batch. We push it once at first flush and once per
    /// flush that carries a [`ViewEvent::PresetCorpusChanged`].
    pub fn flush_view_events(&self) {
        let events = std::mem::take(&mut *self.buf.borrow_mut());
        let needs_corpus = !self.corpus_seeded.get()
            || events
                .iter()
                .any(|e| matches!(e, ViewEvent::PresetCorpusChanged { .. }));
        if events.is_empty() && !needs_corpus {
            return;
        }
        if needs_corpus {
            if let Some(json) = self.serialize_corpus() {
                let js = format!(
                    "if(window.__vxn&&window.__vxn.applyPresetCorpus){{window.__vxn.applyPresetCorpus({json})}}"
                );
                let _ = self.webview.evaluate_script(&js);
                self.corpus_seeded.set(true);
            }
        }
        if events.is_empty() {
            return;
        }
        for chunk_json in batch_chunks(&events, MAX_BATCH_BYTES) {
            let js = format!(
                "if(window.__vxn&&window.__vxn.applyViewEvents){{window.__vxn.applyViewEvents({chunk_json})}}"
            );
            let _ = self.webview.evaluate_script(&js);
        }
    }

    /// Build the JSON corpus payload from the shared snapshot. Returns `None`
    /// if the mutex was poisoned (caller skips the push; next flush retries).
    fn serialize_corpus(&self) -> Option<String> {
        let corpus = self.corpus.lock().ok()?;
        Some(corpus_snapshot_json(&corpus))
    }

    /// Marker for shape parity with vizia's `WindowHandle::close` — the
    /// clack shell calls this from `gui.destroy()`. wry's `WebView::Drop`
    /// already removes the subview from the parent NSView on macOS, so the
    /// real teardown happens when the host drops the handle.
    pub fn close(&mut self) {}
}

/// Zero-sized type that names this backend for trait-bounded code (the clack
/// shell, tests). All state lives in [`EditorHandle`].
pub struct WebEditor;

impl EditorBackend for WebEditor {
    type Handle = EditorHandle;
    /// Raw native parent: NSView pointer on macOS, HWND on Windows, xcb window
    /// id (zero-extended into a pointer slot) on Linux. The clack shell
    /// already extracts these per-platform in `gui::set_parent`.
    type ParentWindow = *mut c_void;

    fn open(
        parent: Self::ParentWindow,
        ctrl: ControllerHandle,
        corpus: CorpusHandle,
    ) -> Self::Handle {
        open_editor(parent, ctrl, corpus)
    }

    fn close(handle: &mut Self::Handle) {
        // Tear down by replacing the handle's WebView with… nothing useful.
        // The host owns the `EditorHandle`; close() is typically just a
        // marker call before drop, so we don't reach into wry internals.
        let _ = handle;
    }

    fn push_view_event(handle: &Self::Handle, event: ViewEvent) {
        handle.push_view_event(event);
    }

    fn flush_view_events(handle: &Self::Handle) {
        handle.flush_view_events();
    }
}

/// Build the WebView under `parent`, wire the IPC handler to `ctrl`, and load
/// the faceplate page. `parent` is the same raw pointer the host hands the
/// clack shell in `gui::set_parent` (NSView / HWND / xcb-window-id).
pub fn open_editor(
    parent: *mut c_void,
    ctrl: ControllerHandle,
    corpus: CorpusHandle,
) -> EditorHandle {
    let parent_raw = parent;
    let parent_wrap = ParentWindow { raw: build_raw(parent_raw) };
    let html = build_faceplate_html();
    let ipc_ctrl = ctrl.clone();
    let webview = WebViewBuilder::new_as_child(&parent_wrap)
        .with_html(html)
        .with_bounds(Rect {
            position: LogicalPosition::new(0i32, 0i32).into(),
            size: LogicalSize::new(EDITOR_WIDTH, EDITOR_HEIGHT).into(),
        })
        .with_ipc_handler(move |req| {
            if let Some(ev) = parse_ui_event(req.body()) {
                let _ = ipc_ctrl.post(ev);
            }
        })
        .build()
        .expect("wry WebView build failed");
    EditorHandle {
        webview,
        buf: RefCell::new(Vec::new()),
        parent: parent_raw,
        ctrl,
        corpus,
        corpus_seeded: Cell::new(false),
    }
}

/// Splice the runtime param-descriptor JSON into the faceplate template. The
/// page reads it as `window.vxn.params = {...}`, a CLAP-id-keyed map of
/// `{name, label, kind, min, max, default, taper, unit, variants?}`. JSON
/// generation is one place so a future schema bump (e.g. adding a `module`
/// field) stays self-contained.
///
/// CSS + the three JS modules (bridge / panels / dispatch) live in sibling
/// files spliced in here — the wry WebView serves the page via `with_html`,
/// so external `<link href>` / `<script src>` would need a custom protocol
/// handler to resolve. Inlining keeps the page self-contained without that
/// plumbing. JS splice order matters: bridge defines `window.vxn` /
/// `__vxn` / `valuePop` / `statusPill`, panels register controls and
/// browser/preset/keys UI against that bridge, dispatch wires `init()` and
/// the ViewEvent fan-out last.
fn build_faceplate_html() -> String {
    PLACEHOLDER_HTML
        .replace("__CSS__", FACEPLATE_CSS)
        .replace("__BRIDGE_JS__", &strip_esm_exports(BRIDGE_JS))
        .replace("__BROWSER_JS__", &strip_esm_exports(BROWSER_JS))
        .replace("__PANELS_JS__", &strip_esm_exports(PANELS_JS))
        .replace("__DISPATCH_JS__", &strip_esm_exports(DISPATCH_JS))
        .replace("__PARAMS_JSON__", &build_params_json())
        .replace("__SUBDIVISIONS_JSON__", &build_subdivisions_json())
        .replace("__PATCH_COUNT__", &PATCH_COUNT.to_string())
}

/// Drop ESM module syntax from every line of `src`. The four faceplate JS
/// modules carry `export` markers (and a couple of cross-module `import`s
/// since E015 / 0079) so Node can load them for the test suite; the splice
/// loader concatenates them into one inline `<script>` where module syntax
/// is illegal, so we strip per line before splicing. `export const X = …`
/// becomes `const X = …` (bare declaration — exactly what these files were
/// before E015); `import { ... } from '...';` becomes an empty line (the
/// splice already puts every binding in one shared scope, so cross-module
/// refs resolve without the import).
fn strip_esm_exports(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for (i, line) in src.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Imports drop to blank lines to keep line counts stable for
        // stack traces — concat-side scope already has the bindings.
        if line.starts_with("import ") {
            continue;
        }
        let stripped = line
            .strip_prefix("export default ")
            .or_else(|| line.strip_prefix("export "))
            .unwrap_or(line);
        out.push_str(stripped);
    }
    if src.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Tempo-sync subdivision labels (vxn_app::sync::SUBDIVISIONS), spliced into
/// the page as `window.vxn.subdivisions`. The LFO-rate fader's display reads
/// from this list when its sync partner is on (0042 / 0015) — matches the
/// vizia editor's `sync_partner` override, which indexes the same table.
fn build_subdivisions_json() -> String {
    let labels: Vec<String> = vxn_app::sync::SUBDIVISIONS
        .iter()
        .map(|s| format!("\"{}\"", s.label))
        .collect();
    format!("[{}]", labels.join(","))
}

fn build_params_json() -> String {
    let entries: Vec<String> = (0..TOTAL_PARAMS)
        .filter_map(|id| desc_for_clap_id(id).map(|d| (id, d)))
        .map(|(id, d)| format!(r#""{id}":{}"#, descriptor_to_json(d)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn descriptor_to_json(d: &ParamDesc) -> String {
    use serde_json::json;
    let mut v = json!({
        "name": d.name,
        "label": d.label,
        "min": d.min,
        "max": d.max,
        "default": d.default,
    });
    let obj = v.as_object_mut().expect("json object");
    match d.kind {
        ParamKind::Float { unit, taper } => {
            obj.insert("kind".into(), json!("float"));
            obj.insert("unit".into(), json!(unit));
            obj.insert("taper".into(), json!(taper_to_json(taper)));
        }
        ParamKind::Int { unit } => {
            obj.insert("kind".into(), json!("int"));
            obj.insert("unit".into(), json!(unit));
        }
        ParamKind::Bool => {
            obj.insert("kind".into(), json!("bool"));
        }
        ParamKind::Enum { variants } => {
            obj.insert("kind".into(), json!("enum"));
            obj.insert("variants".into(), json!(variants));
        }
    }
    v.to_string()
}

fn taper_to_json(t: vxn_app::Taper) -> serde_json::Value {
    use serde_json::json;
    match t {
        vxn_app::Taper::Linear => json!({"kind": "linear"}),
        vxn_app::Taper::Exp { mid } => json!({"kind": "exp", "mid": mid}),
    }
}

// ── Parent-window adapter ───────────────────────────────────────────────────

/// Newtype that lets a raw native parent pointer satisfy
/// [`HasWindowHandle`]. The host owns the underlying window for the editor's
/// lifetime — we never outlive it.
struct ParentWindow {
    raw: RawWindowHandle,
}

// `RawWindowHandle` is `!Send`/`!Sync`; wry doesn't require either on the
// `HasWindowHandle` impl, but the bounds aren't expressible without these
// unsafe asserts on some toolchains. Safe here because we hand the parent
// straight to wry on the same thread and never share it.
unsafe impl Send for ParentWindow {}
unsafe impl Sync for ParentWindow {}

impl HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<RwhWindowHandle<'_>, HandleError> {
        // SAFETY: `raw` was built from the host-provided native handle; it
        // stays valid as long as the host hasn't destroyed the GUI, which
        // strictly outlives every borrow wry takes here.
        Ok(unsafe { RwhWindowHandle::borrow_raw(self.raw) })
    }
}

#[cfg(target_os = "macos")]
fn build_raw(ptr: *mut c_void) -> RawWindowHandle {
    use raw_window_handle::AppKitWindowHandle;
    use std::ptr::NonNull;
    let ns_view = NonNull::new(ptr).expect("parent NSView is null");
    RawWindowHandle::AppKit(AppKitWindowHandle::new(ns_view))
}

#[cfg(target_os = "windows")]
fn build_raw(ptr: *mut c_void) -> RawWindowHandle {
    use raw_window_handle::Win32WindowHandle;
    use std::num::NonZeroIsize;
    let hwnd = NonZeroIsize::new(ptr as isize).expect("parent HWND is zero");
    RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn build_raw(ptr: *mut c_void) -> RawWindowHandle {
    use raw_window_handle::XcbWindowHandle;
    use std::num::NonZeroU32;
    // The clack shell hands us the xcb window id zero-extended into a pointer
    // slot; truncate back to u32. Matches `gui::set_parent`.
    let win = NonZeroU32::new(ptr as usize as u32).expect("parent xcb window is zero");
    RawWindowHandle::Xcb(XcbWindowHandle::new(win))
}

// ── IPC inbound: JSON → UiEvent ─────────────────────────────────────────────

/// Parse one IPC message into a [`UiEvent`]. Returns `None` for malformed
/// payloads or unknown opcodes (logged silently — surfacing parse errors is a
/// later ticket).
///
/// Wire shape: `{ "op": "<opcode>", ...fields }`. The opcode set below is the
/// minimum that lets 0041+ wire faders, transport, layer toggles, and
/// factory-bank loads against the controller. Path-based preset mutations
/// (save / rename / move / delete) join in 0049–0051 once the browser HTML
/// lands.
fn parse_ui_event(body: &str) -> Option<UiEvent> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let op = v.get("op")?.as_str()?;
    match op {
        "set_param" => Some(UiEvent::SetParam {
            id: ParamId::new(v.get("id")?.as_u64()? as usize),
            plain: v.get("plain")?.as_f64()? as f32,
        }),
        "set_param_norm" => Some(UiEvent::SetParamNorm {
            id: ParamId::new(v.get("id")?.as_u64()? as usize),
            norm: v.get("norm")?.as_f64()? as f32,
        }),
        "begin_gesture" => Some(UiEvent::BeginGesture {
            id: ParamId::new(v.get("id")?.as_u64()? as usize),
        }),
        "end_gesture" => Some(UiEvent::EndGesture {
            id: ParamId::new(v.get("id")?.as_u64()? as usize),
        }),
        "reset_layer" => Some(UiEvent::ResetLayer {
            layer: parse_layer(v.get("layer")?)?,
        }),
        "load_factory" => Some(UiEvent::LoadPreset {
            source: PresetSource::Factory {
                index: v.get("index")?.as_u64()? as usize,
            },
        }),
        // 0050: browser panel posts this when the user clicks a user-side
        // preset row. `path` is the absolute on-disk path the corpus
        // snapshot ships (`p.path` in `corpus_snapshot_json`).
        "load_user" => Some(UiEvent::LoadPreset {
            source: PresetSource::User {
                path: std::path::PathBuf::from(v.get("path")?.as_str()?.to_owned()),
            },
        }),
        // 0051: user-side mutation ops. Paths round-trip as strings the
        // controller maps back to `PathBuf`; the controller is the only
        // place that touches disk (refreshes the corpus and re-emits
        // `PresetCorpusChanged` after each).
        "rename_preset" => Some(UiEvent::RenamePreset {
            path: std::path::PathBuf::from(v.get("path")?.as_str()?.to_owned()),
            new_name: v.get("new_name")?.as_str()?.to_owned(),
        }),
        "delete_preset" => Some(UiEvent::DeletePreset {
            path: std::path::PathBuf::from(v.get("path")?.as_str()?.to_owned()),
        }),
        "move_preset" => Some(UiEvent::MovePreset {
            path: std::path::PathBuf::from(v.get("path")?.as_str()?.to_owned()),
            // `dest_folder: null` moves to user root; any string names the
            // destination subfolder.
            dest_folder: v
                .get("dest_folder")
                .and_then(|x| x.as_str())
                .map(str::to_owned),
        }),
        "rename_folder" => Some(UiEvent::RenameFolder {
            old_name: v.get("old_name")?.as_str()?.to_owned(),
            new_name: v.get("new_name")?.as_str()?.to_owned(),
        }),
        "delete_folder" => Some(UiEvent::DeleteFolder {
            name: v.get("name")?.as_str()?.to_owned(),
        }),
        "new_folder" => Some(UiEvent::NewFolder {
            suggested: v.get("suggested")?.as_str()?.to_owned(),
        }),
        // 0049: prev/next walker. `delta` is signed; the controller wraps
        // against the combined Factory + User list it publishes.
        "step_preset" => Some(UiEvent::StepPreset {
            delta: v.get("delta")?.as_i64()? as i32,
        }),
        // 0049: Save As — name from the floating popup, folder from the
        // browser panel's selection (0050+). For 0049 the page sends
        // `folder: null` unconditionally → saves to user root.
        "save_preset" => Some(UiEvent::SavePreset {
            name: v.get("name")?.as_str()?.to_owned(),
            folder: v.get("folder").and_then(|x| x.as_str()).map(str::to_owned),
        }),
        "set_key_mode" => Some(UiEvent::SetKeyMode {
            mode: parse_key_mode(v.get("mode")?)?,
        }),
        "set_split_point" => Some(UiEvent::SetSplitPoint {
            note: v.get("note")?.as_u64()? as u8,
        }),
        "set_edit_layer" => Some(UiEvent::SetEditLayer {
            layer: parse_layer(v.get("layer")?)?,
        }),
        // Sent by the page's `init()` once the JS dispatcher is wired.
        // Triggers a controller-side broadcast so any param/key-mode
        // ViewEvents that raced ahead of the bootstrap script get re-sent
        // into a known-ready listener.
        "ready" => Some(UiEvent::EditorReady),
        // 0048: faceplate asks for the floating text-input popup. The
        // controller relays this as `ViewEvent::OpenTextInput`; the
        // editor backend intercepts and pops the native window.
        "request_text_input" => Some(UiEvent::RequestTextInput {
            id: v.get("id")?.as_str()?.to_owned(),
            title: v.get("title")?.as_str()?.to_owned(),
            initial: v.get("initial")?.as_str().unwrap_or("").to_owned(),
        }),
        // Reserved for direct page-side posts (in-page tests, or a future
        // platform where the popup lives JS-side). Production flow on
        // macOS routes through the native popup → `ctrl.post` instead.
        "text_input_result" => Some(UiEvent::TextInputResult {
            id: v.get("id")?.as_str()?.to_owned(),
            value: v.get("value").and_then(|x| x.as_str()).map(|s| s.to_owned()),
        }),
        _ => None,
    }
}

fn parse_layer(v: &serde_json::Value) -> Option<Layer> {
    match v.as_str()? {
        "upper" => Some(Layer::Upper),
        "lower" => Some(Layer::Lower),
        _ => None,
    }
}

fn parse_key_mode(v: &serde_json::Value) -> Option<KeyMode> {
    Some(KeyMode::from_u8(v.as_u64()? as u8))
}

// ── ViewEvent → JSON batches ────────────────────────────────────────────────

/// Dedupe `ParamChanged` events by id (latest value wins, preserves the
/// position of the last occurrence relative to non-`ParamChanged` events).
/// Other variants pass through unchanged. Bounded at `events.len()`; the
/// hashmap is reused across calls is not worth it here — buffers are short.
fn dedup_param_changes(events: &[ViewEvent]) -> Vec<&ViewEvent> {
    let mut latest_for_id: HashMap<usize, usize> = HashMap::new();
    for (i, ev) in events.iter().enumerate() {
        if let ViewEvent::ParamChanged { id, .. } = ev {
            latest_for_id.insert(id.raw(), i);
        }
    }
    events
        .iter()
        .enumerate()
        .filter(|(i, ev)| match ev {
            ViewEvent::ParamChanged { id, .. } => latest_for_id.get(&id.raw()) == Some(i),
            _ => true,
        })
        .map(|(_, ev)| ev)
        .collect()
}

/// Build one or more JSON-array literals from a tick batch. Each chunk is a
/// `[...]` string ≤ `max_bytes` (a single event larger than `max_bytes`
/// still ships on its own — splitting inside a JSON object would corrupt
/// the page).
fn batch_chunks(events: &[ViewEvent], max_bytes: usize) -> Vec<String> {
    let deduped = dedup_param_changes(events);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::from("[");
    let mut first_in_chunk = true;
    for ev in deduped {
        let s = view_event_to_json(ev);
        let projected = current.len() + s.len() + if first_in_chunk { 1 } else { 2 };
        if !first_in_chunk && projected > max_bytes {
            current.push(']');
            chunks.push(std::mem::replace(&mut current, String::from("[")));
            first_in_chunk = true;
        }
        if !first_in_chunk {
            current.push(',');
        }
        current.push_str(&s);
        first_in_chunk = false;
    }
    current.push(']');
    if current != "[]" {
        chunks.push(current);
    }
    chunks
}

/// Serialize a [`ViewEvent`] to a JSON value the page can read. Mirror of
/// [`parse_ui_event`]'s opcode shape: `{ "kind": "...", ...fields }`.
fn view_event_to_json(ev: &ViewEvent) -> String {
    use serde_json::json;
    let v = match ev {
        ViewEvent::ParamChanged { id, plain, norm, display } => json!({
            "kind": "param_changed",
            "id": id.raw(),
            "plain": plain,
            "norm": norm,
            "display": display,
        }),
        ViewEvent::PresetLoaded { meta, source, warnings } => json!({
            "kind": "preset_loaded",
            "name": meta.name,
            "source": preset_source_json(source.as_ref()),
            "warnings": warnings,
        }),
        ViewEvent::PresetCorpusChanged { follow } => json!({
            "kind": "preset_corpus_changed",
            "follow": follow.as_ref().map(|p| p.display().to_string()),
        }),
        ViewEvent::KeyModeChanged { mode } => json!({
            "kind": "key_mode_changed",
            "mode": *mode as u8,
        }),
        ViewEvent::SplitPointChanged { note } => json!({
            "kind": "split_point_changed",
            "note": *note,
        }),
        ViewEvent::EditLayerChanged { layer } => json!({
            "kind": "edit_layer_changed",
            "layer": match layer { Layer::Upper => "upper", Layer::Lower => "lower" },
        }),
        ViewEvent::Status { line } => json!({
            "kind": "status",
            "line": line,
        }),
        // OpenTextInput is intercepted in `push_view_event` before
        // batching, so this arm is unreachable on the happy path. Serialize
        // a benign marker rather than `panic!` so a future refactor that
        // leaks it into the buffer fails closed (JS dispatcher ignores
        // unknown `kind`s).
        ViewEvent::OpenTextInput { id, title, initial } => json!({
            "kind": "open_text_input",
            "id": id,
            "title": title,
            "initial": initial,
        }),
        ViewEvent::TextInputResult { id, value } => json!({
            "kind": "text_input_result",
            "id": id,
            "value": value,
        }),
    };
    v.to_string()
}

/// Serialize a [`PresetCorpus`] for the JS browser panel (0050). Factory
/// presets are grouped by `meta.category` (presets without a category fall
/// into [`UNCATEGORIZED`]); user folders preserve their `Option<String>`
/// shape so the page can show "Uncategorised" first then sorted named
/// folders. Within each group, presets are alpha-sorted by name
/// (case-insensitive) — same order the prev/next walker uses.
fn corpus_snapshot_json(corpus: &PresetCorpus) -> String {
    use serde_json::{Value, json};

    let mut factory_groups: HashMap<String, Vec<(usize, &str)>> = HashMap::new();
    for (i, m) in corpus.factory.iter().enumerate() {
        let cat = m
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(UNCATEGORIZED)
            .to_string();
        factory_groups
            .entry(cat)
            .or_default()
            .push((i, m.name.as_str()));
    }
    let mut factory: Vec<(String, Vec<(usize, &str)>)> = factory_groups.into_iter().collect();
    factory.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    for g in factory.iter_mut() {
        g.1.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    }
    let factory_v: Vec<Value> = factory
        .into_iter()
        .map(|(category, presets)| {
            let entries: Vec<Value> = presets
                .into_iter()
                .map(|(idx, name)| json!({"name": name, "index": idx}))
                .collect();
            json!({"category": category, "presets": entries})
        })
        .collect();

    let mut user = corpus.user.clone();
    user.sort_by(|a, b| match (&a.name, &b.name) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
    });
    let user_v: Vec<Value> = user
        .into_iter()
        .map(|f| {
            let mut presets = f.presets;
            presets.sort_by(|a, b| a.meta.name.to_lowercase().cmp(&b.meta.name.to_lowercase()));
            let entries: Vec<Value> = presets
                .into_iter()
                .map(|p| {
                    json!({"name": p.meta.name, "path": p.path.display().to_string()})
                })
                .collect();
            json!({"name": f.name, "presets": entries})
        })
        .collect();
    json!({"factory": factory_v, "user": user_v}).to_string()
}

fn preset_source_json(src: Option<&PresetSource>) -> serde_json::Value {
    use serde_json::json;
    match src {
        None => serde_json::Value::Null,
        Some(PresetSource::Factory { index }) => json!({"kind": "factory", "index": index}),
        Some(PresetSource::User { path }) => json!({"kind": "user", "path": path.display().to_string()}),
    }
}

// ── Faceplate page ──────────────────────────────────────────────────────────

/// Faceplate HTML scaffold (0040). Four-row panel grid; controls populated
/// at runtime by the JS modules. The HTML carries placeholders for the CSS
/// and the three JS modules so each file stays editable on its own without
/// hunting for the boundaries inside a 3500-line blob — `build_faceplate_html`
/// splices them back together at editor-open time.
const PLACEHOLDER_HTML: &str = include_str!("../assets/faceplate.html");
/// Stylesheet — spliced into the `<style>__CSS__</style>` slot of the HTML.
const FACEPLATE_CSS: &str = include_str!("../assets/faceplate.css");
/// IPC bootstrap + shared UI scaffolding (`window.vxn` / `window.__vxn`,
/// text-input bridge, value popup, status pill). Defines the globals every
/// later module relies on, so it splices first inside `<script>`.
const BRIDGE_JS: &str = include_str!("../assets/bridge.js");
/// Preset browser panel — corpus model, folder/preset rendering, search,
/// context menu, modal confirms (delete + save-as), DnD. Splices between
/// bridge and the rest of panels because the bar IIFE
/// (`const presetBar = …`) references `browserPanel`.
const BROWSER_JS: &str = include_str!("../assets/browser.js");
/// Panel UI — preset bar, Keys panel, waveform glyphs, control primitives
/// (fader / wave / switch / buttongroup / dropdown / header-switch /
/// detune-legato). Registers everything against `model.controls` so
/// dispatch can fan ViewEvents to the right cell.
const PANELS_JS: &str = include_str!("../assets/panels.js");
/// `init()` + per-tick ViewEvent dispatcher + dim rules + layer rebind.
/// Splices last because it references the panel objects defined above.
const DISPATCH_JS: &str = include_str!("../assets/dispatch.js");

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vxn_app::PresetMeta;

    #[test]
    fn parses_set_param_norm() {
        let ev = parse_ui_event(r#"{"op":"set_param_norm","id":42,"norm":0.5}"#).unwrap();
        match ev {
            UiEvent::SetParamNorm { id, norm } => {
                assert_eq!(id.raw(), 42);
                assert!((norm - 0.5).abs() < 1e-6);
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
    }

    #[test]
    fn parses_factory_load() {
        let ev = parse_ui_event(r#"{"op":"load_factory","index":7}"#).unwrap();
        match ev {
            UiEvent::LoadPreset { source: PresetSource::Factory { index } } => {
                assert_eq!(index, 7);
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
    }

    #[test]
    fn parses_mutation_ops() {
        // 0051: each of the user-side mutation flows posts a dedicated
        // op. The controller already handles the matching UiEvents.
        let ev = parse_ui_event(
            r#"{"op":"rename_preset","path":"/u/x.preset","new_name":"Y"}"#,
        ).unwrap();
        match ev {
            UiEvent::RenamePreset { path, new_name } => {
                assert_eq!(path, PathBuf::from("/u/x.preset"));
                assert_eq!(new_name, "Y");
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
        let ev = parse_ui_event(r#"{"op":"delete_preset","path":"/u/x.preset"}"#).unwrap();
        assert!(matches!(ev, UiEvent::DeletePreset { ref path } if path == &PathBuf::from("/u/x.preset")));
        let ev = parse_ui_event(
            r#"{"op":"move_preset","path":"/u/x.preset","dest_folder":"Bass"}"#,
        ).unwrap();
        match ev {
            UiEvent::MovePreset { path, dest_folder } => {
                assert_eq!(path, PathBuf::from("/u/x.preset"));
                assert_eq!(dest_folder.as_deref(), Some("Bass"));
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
        // dest_folder: null routes to user root.
        let ev = parse_ui_event(
            r#"{"op":"move_preset","path":"/u/x.preset","dest_folder":null}"#,
        ).unwrap();
        assert!(matches!(
            ev,
            UiEvent::MovePreset { dest_folder: None, .. },
        ));
        let ev = parse_ui_event(
            r#"{"op":"rename_folder","old_name":"Bass","new_name":"Bassline"}"#,
        ).unwrap();
        match ev {
            UiEvent::RenameFolder { old_name, new_name } => {
                assert_eq!(old_name, "Bass");
                assert_eq!(new_name, "Bassline");
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
        let ev = parse_ui_event(r#"{"op":"delete_folder","name":"Bass"}"#).unwrap();
        assert!(matches!(ev, UiEvent::DeleteFolder { ref name } if name == "Bass"));
        let ev = parse_ui_event(r#"{"op":"new_folder","suggested":"Pads"}"#).unwrap();
        assert!(matches!(ev, UiEvent::NewFolder { ref suggested } if suggested == "Pads"));
    }

    #[test]
    fn parses_user_load() {
        // 0050: browser panel posts `load_user` with the absolute path
        // from the corpus snapshot when the user clicks a user-side row.
        let ev = parse_ui_event(r#"{"op":"load_user","path":"/u/p/Bass/My Patch.preset"}"#).unwrap();
        match ev {
            UiEvent::LoadPreset { source: PresetSource::User { path } } => {
                assert_eq!(path, PathBuf::from("/u/p/Bass/My Patch.preset"));
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
    }

    #[test]
    fn parses_layer_and_key_mode() {
        let ev = parse_ui_event(r#"{"op":"set_edit_layer","layer":"lower"}"#).unwrap();
        assert!(matches!(ev, UiEvent::SetEditLayer { layer: Layer::Lower }));
        let ev = parse_ui_event(r#"{"op":"set_key_mode","mode":2}"#).unwrap();
        assert!(matches!(ev, UiEvent::SetKeyMode { mode: KeyMode::Split }));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_ui_event("not json").is_none());
        assert!(parse_ui_event(r#"{"op":"unknown"}"#).is_none());
        assert!(parse_ui_event(r#"{"op":"set_param_norm","id":42}"#).is_none());
    }

    #[test]
    fn parses_step_preset_signed_delta() {
        // 0049: prev posts -1, next posts +1. delta is signed so the parser
        // must accept negative values.
        let ev = parse_ui_event(r#"{"op":"step_preset","delta":-1}"#).unwrap();
        assert!(matches!(ev, UiEvent::StepPreset { delta: -1 }));
        let ev = parse_ui_event(r#"{"op":"step_preset","delta":1}"#).unwrap();
        assert!(matches!(ev, UiEvent::StepPreset { delta: 1 }));
    }

    #[test]
    fn parses_save_preset_with_and_without_folder() {
        // 0049: Save As. `folder: null` saves to user root; a string
        // names the destination subfolder (0050+ sources this from the
        // browser panel's selection).
        let ev = parse_ui_event(
            r#"{"op":"save_preset","name":"Pad 1","folder":null}"#,
        )
        .unwrap();
        match ev {
            UiEvent::SavePreset { name, folder } => {
                assert_eq!(name, "Pad 1");
                assert!(folder.is_none());
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
        let ev = parse_ui_event(
            r#"{"op":"save_preset","name":"Brassy","folder":"Lead"}"#,
        )
        .unwrap();
        match ev {
            UiEvent::SavePreset { name, folder } => {
                assert_eq!(name, "Brassy");
                assert_eq!(folder.as_deref(), Some("Lead"));
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
    }

    fn param_changed(id: usize, plain: f32) -> ViewEvent {
        ViewEvent::ParamChanged {
            id: ParamId::new(id),
            plain,
            norm: plain,
            display: format!("{plain}"),
        }
    }

    #[test]
    fn dedup_keeps_latest_param_per_id() {
        // Three writes to id 1 in a tick → only the last one ships.
        let events = vec![
            param_changed(1, 0.1),
            param_changed(2, 0.2),
            param_changed(1, 0.3),
            param_changed(1, 0.4),
            ViewEvent::Status { line: "ok".into() },
            param_changed(2, 0.5),
        ];
        let kept: Vec<f32> = dedup_param_changes(&events)
            .into_iter()
            .filter_map(|ev| match ev {
                ViewEvent::ParamChanged { plain, .. } => Some(*plain),
                _ => None,
            })
            .collect();
        assert_eq!(kept, vec![0.4, 0.5]);
        // Non-ParamChanged variants survive untouched.
        let kinds: Vec<_> = dedup_param_changes(&events)
            .into_iter()
            .map(|ev| matches!(ev, ViewEvent::Status { .. }))
            .collect();
        assert!(kinds.iter().any(|x| *x), "Status must be kept");
    }

    #[test]
    fn batch_chunks_single_under_cap() {
        let events = vec![param_changed(1, 0.5), param_changed(2, 0.5)];
        let chunks = batch_chunks(&events, 10_000);
        assert_eq!(chunks.len(), 1, "should fit in one chunk");
        let v: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["kind"], "param_changed");
    }

    #[test]
    fn batch_chunks_splits_above_cap() {
        // 200 distinct ids — each event JSON is ~80 bytes, so a tight cap
        // forces multiple chunks. Every chunk must parse as a JSON array,
        // and concatenating their contents must equal the deduped input.
        let events: Vec<ViewEvent> = (0..200).map(|i| param_changed(i, i as f32 * 0.01)).collect();
        let chunks = batch_chunks(&events, 1_000);
        assert!(chunks.len() > 1, "tight cap should split: got {}", chunks.len());
        let mut total = 0;
        for c in &chunks {
            let v: serde_json::Value = serde_json::from_str(c).unwrap();
            let arr = v.as_array().expect("array");
            total += arr.len();
            assert!(c.len() <= 1_000 + 200, "chunk size respects cap (slack: {})", c.len());
        }
        assert_eq!(total, 200, "all events present across chunks");
    }

    #[test]
    fn batch_chunks_empty_yields_nothing() {
        assert!(batch_chunks(&[], 10_000).is_empty());
    }

    #[test]
    fn batch_chunks_dedup_applies_before_chunking() {
        // Two writes to the same id collapse before chunking.
        let events = vec![param_changed(1, 0.1), param_changed(1, 0.9)];
        let chunks = batch_chunks(&events, 10_000);
        assert_eq!(chunks.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert!((arr[0]["plain"].as_f64().unwrap() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn view_event_serializes() {
        let s = view_event_to_json(&ViewEvent::ParamChanged {
            id: ParamId::new(3),
            plain: 1.25,
            norm: 0.5,
            display: "1.25 Hz".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "param_changed");
        assert_eq!(v["id"], 3);
        assert_eq!(v["display"], "1.25 Hz");
    }

    #[test]
    fn preset_loaded_serializes_factory_source() {
        let s = view_event_to_json(&ViewEvent::PresetLoaded {
            meta: PresetMeta { name: "Brassy".into(), ..Default::default() },
            source: Some(PresetSource::Factory { index: 12 }),
            warnings: vec!["clamped".into()],
        });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "preset_loaded");
        assert_eq!(v["name"], "Brassy");
        assert_eq!(v["source"]["kind"], "factory");
        assert_eq!(v["source"]["index"], 12);
        assert_eq!(v["warnings"][0], "clamped");
    }

    #[test]
    fn corpus_changed_serializes_follow_path() {
        let s = view_event_to_json(&ViewEvent::PresetCorpusChanged {
            follow: Some(PathBuf::from("/tmp/x.preset")),
        });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "preset_corpus_changed");
        assert_eq!(v["follow"], "/tmp/x.preset");
    }

    // ── Faceplate structural checks (0040) ─────────────────────────────────
    //
    // Substring-only — pulling an HTML parser in just to assert presence
    // would be overkill. The asserts here catch silent regressions (a row
    // dropped, a panel renamed, a data attr lost) without pinning markup.

    // Assemble once per test run — `build_faceplate_html` walks every CLAP
    // id to build the descriptor map, so caching keeps the structural-check
    // suite under a millisecond instead of paying that per test.
    fn assembled() -> &'static str {
        use std::sync::OnceLock;
        static CACHED: OnceLock<String> = OnceLock::new();
        CACHED.get_or_init(build_faceplate_html).as_str()
    }

    fn count(needle: &str) -> usize {
        assembled().matches(needle).count()
    }

    #[test]
    fn faceplate_has_banner_and_preset_bar_slot() {
        assert!(assembled().contains(r#"class="banner""#));
        assert!(assembled().contains("VULPUS LABS"));
        assert!(assembled().contains("VXN-1"));
        assert!(assembled().contains(r#"class="preset-bar-slot""#));
    }

    #[test]
    fn faceplate_has_four_rows() {
        for r in 1..=4 {
            assert!(
                assembled().contains(&format!(r#"data-row="{r}""#)),
                "missing data-row=\"{r}\"",
            );
        }
        // Rows 1-3 = 5 panels each; row 4 = 6 panels (E012 / 0058 added Reverb).
        // 5+5+5+6 = 21. Catches an accidental row collapse or duplicate emit.
        assert_eq!(count(r#"class="panel""#), 21, "panel count drift");
    }

    #[test]
    fn faceplate_panel_names_match_rows() {
        // Same titles as `vxn_ui_vizia::ROWS`; reordering or rename would have to
        // happen here in lockstep. Reverb (E012 / 0058) lives in row 4 between
        // Delay and Master.
        let expected: &[&[&str]] = &[
            &["LFO 1", "LFO 2", "Osc 1", "Osc 2", "Mixer"],
            &["Env 1", "Env 2", "VCA", "Filter", "Filter Mod"],
            &["Pitch Mod", "PWM Mod", "Cross Mod", "Mod Wheel", "Bend"],
            &["Keys", "Voice", "Chorus", "Delay", "Reverb", "Master"],
        ];
        for row in expected {
            for name in *row {
                assert!(
                    assembled().contains(&format!(r#"data-name="{name}""#)),
                    "missing panel {name}",
                );
            }
        }
    }

    #[test]
    fn faceplate_layered_panels_match_vxn_ui_vizia() {
        // Layered = panel has at least one per-patch (Upper/Lower) entry in
        // `vxn_ui_vizia::ROWS`. Mirror that list here so we notice if a panel's
        // entry mix changes upstream.
        let layered = [
            "LFO 1", "Osc 1", "Osc 2", "Mixer", "Env 1", "Env 2", "VCA",
            "Filter", "Filter Mod", "Pitch Mod", "PWM Mod", "Cross Mod",
            "Mod Wheel", "Bend", "Voice",
        ];
        for name in layered {
            let marker = format!(r#"data-name="{name}" data-layered"#);
            assert!(
                assembled().contains(&marker),
                "panel {name} missing data-layered",
            );
        }
        // Count attribute occurrences only — `data-layered>` skips the CSS
        // `[data-layered]` selector hit.
        assert_eq!(
            count("data-layered>"),
            layered.len(),
            "extra/missing data-layered panel",
        );
    }

    #[test]
    fn faceplate_reserves_fx_header_toggles() {
        // Header switch idiom: Chorus + Delay (0045), Reverb (E012 / 0058).
        for name in ["Chorus", "Delay", "Reverb"] {
            assert!(
                assembled()
                    .contains(&format!(r#"data-name="{name}" data-header-toggle"#)),
                "{name} missing data-header-toggle",
            );
        }
        // `data-header-toggle>` matches the panel-div attribute only;
        // CSS `[data-header-toggle]` selectors don't have the closing `>`.
        assert_eq!(
            count("data-header-toggle>"),
            3,
            "header-toggle expected on Chorus + Delay + Reverb only",
        );
    }

    #[test]
    fn faceplate_css_vars_match_vxn_ui_vizia_constants() {
        // Pixel literals live in CSS vars (ticket: "a future resize policy
        // should be one variable change"). Sanity check the load-bearing
        // ones against `vxn_ui_vizia` constants.
        assert!(assembled().contains("--panel-h: 156px"));
        assert!(assembled().contains("--col-h: 120px"));
        assert!(assembled().contains("--fader-h: 74px"));
        assert!(assembled().contains("--dial: 62px"));
        assert!(assembled().contains("--banner-h: 26px"));
        assert!(assembled().contains("--preset-bar-h: 30px"));
        assert!(assembled().contains("--pad-outer: 10px"));
    }

    #[test]
    fn faceplate_row_panel_widths_match_vizia() {
        // Stretch shares from `vxn_ui_vizia::panel_view`'s `match title` block. If
        // upstream tweaks a share, this fails — keeping the HTML pinned to
        // the vizia layout the user already approved.
        for (sel, share) in [
            ("LFO 1", "1.2"),
            ("LFO 2", "0.7"),
            ("Osc 1", "1.2"),
            ("Osc 2", "1.2"),
            ("Mixer", "1.1"),
            ("Env 1", "0.8"),
            ("Env 2", "0.8"),
            ("VCA", "0.75"),
            ("Filter", "1.15"),
            ("Filter Mod", "1.0"),
        ] {
            assert!(
                assembled()
                    .contains(&format!(r#".panel[data-name="{sel}"]"#))
                    && assembled().contains(&format!("flex-grow: {share}")),
                "share for {sel} ≠ {share}",
            );
        }
        // Bend is the only fixed-width panel.
        assert!(assembled().contains("flex: 0 0 54px"));
    }

    #[test]
    fn faceplate_bridge_object_intact() {
        // Bridge from 0039 still present — 0040 only adds layout.
        assert!(assembled().contains("window.vxn"));
        assert!(assembled().contains("window.ipc.postMessage"));
        assert!(assembled().contains("onViewEvent"));
    }

    #[test]
    fn faceplate_batched_bridge_wired() {
        // 0046: Rust calls `window.__vxn.applyViewEvents(arr)` once per
        // controller tick. Bootstrap installs a buffering stub; init() swaps
        // in the real dispatcher.
        assert!(assembled().contains("window.__vxn"));
        assert!(assembled().contains("applyViewEvents"));
        // Bootstrap stub still funnels into `_earlyViewEvents` so events
        // that race the inline init() are not lost.
        assert!(assembled().contains("_earlyViewEvents"));
    }

    #[test]
    fn faceplate_esm_exports_stripped() {
        // 0076: the four asset files declare ESM `export` markers so Node
        // can `import` them for the E015 test suite, but wry's inline
        // `<script>` slot can't take module syntax. `strip_esm_exports`
        // peels the prefix per line during splice; the assembled HTML
        // must contain no `export ` markers, and the load-bearing
        // declarations (`window.vxn = {`, `function init()`) survive
        // intact under their bare names.
        assert!(
            !assembled().contains("export "),
            "strip_esm_exports left an `export ` marker in the assembled HTML",
        );
        // E015 / 0079: cross-module `import { ... } from './...';` lines
        // must also drop. The strip leaves the line blank so concat-side
        // scope still owns the binding.
        assert!(
            !assembled().contains("import "),
            "strip_esm_exports left an `import ` line in the assembled HTML",
        );
        assert!(assembled().contains("function init()"));
        assert!(assembled().contains("window.vxn = {"));
    }

    #[test]
    fn strip_esm_exports_drops_prefix_per_line() {
        let src = "export const X = 1;\nexport function f() {}\nexport default 7;\nconst Y = 2;\n";
        let out = strip_esm_exports(src);
        assert_eq!(
            out,
            "const X = 1;\nfunction f() {}\n7;\nconst Y = 2;\n",
        );
        // Non-prefix lines pass through; trailing-newline shape preserved.
        let no_trailing = "export const X = 1;";
        assert_eq!(strip_esm_exports(no_trailing), "const X = 1;");
        // E015 / 0079: imports drop to empty lines.
        let with_import = "import { foo } from './bar.js';\nconst X = 1;\n";
        assert_eq!(strip_esm_exports(with_import), "\nconst X = 1;\n");
    }

    #[test]
    fn faceplate_text_input_bridge_wired() {
        // 0048: faceplate exposes `window.vxn.promptText(title, initial,
        // cb)` and the dispatcher routes `text_input_result` back to the
        // pending callback. JS plumbing only — the actual NSWindow is
        // verified by running the plugin in-DAW (see ticket Acceptance).
        assert!(assembled().contains("window.vxn.promptText"));
        assert!(assembled().contains("_textInputCallbacks"));
        assert!(assembled().contains("send.requestTextInput("));
        assert!(assembled().contains("ev.kind === 'text_input_result'"));
    }

    #[test]
    fn parses_request_and_result_text_input() {
        // Faceplate → controller: `request_text_input` carries the
        // correlation id + title + initial.
        let ev = parse_ui_event(
            r#"{"op":"request_text_input","id":"ti1","title":"Rename","initial":"Pad 1"}"#,
        )
        .unwrap();
        match ev {
            UiEvent::RequestTextInput { id, title, initial } => {
                assert_eq!(id, "ti1");
                assert_eq!(title, "Rename");
                assert_eq!(initial, "Pad 1");
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
        // Direct page-side result post (in-page tests): null `value`
        // round-trips as `None`, string round-trips as `Some`.
        let ev = parse_ui_event(r#"{"op":"text_input_result","id":"ti1","value":null}"#).unwrap();
        assert!(matches!(
            ev,
            UiEvent::TextInputResult { ref id, value: None } if id == "ti1"
        ));
        let ev = parse_ui_event(
            r#"{"op":"text_input_result","id":"ti2","value":"new name"}"#,
        )
        .unwrap();
        match ev {
            UiEvent::TextInputResult { id, value } => {
                assert_eq!(id, "ti2");
                assert_eq!(value.as_deref(), Some("new name"));
            }
            _ => panic!("wrong variant: {ev:?}"),
        }
    }

    #[test]
    fn text_input_result_serializes() {
        // Controller → page: commit echoes the string; cancel echoes
        // null (JS dispatcher fires the pending callback with null).
        let s = view_event_to_json(&ViewEvent::TextInputResult {
            id: "ti9".into(),
            value: Some("Pad 1".into()),
        });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "text_input_result");
        assert_eq!(v["id"], "ti9");
        assert_eq!(v["value"], "Pad 1");

        let s = view_event_to_json(&ViewEvent::TextInputResult {
            id: "ti10".into(),
            value: None,
        });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["value"].is_null());
    }

    #[test]
    fn faceplate_status_pill_wired() {
        // 0046: Status ViewEvent flashes the status chip. 0049 re-anchored
        // it from the lower-right corner into the preset bar; the
        // `.status-pill` class + `statusPill.flash` API are unchanged so
        // the bridge contract here stays the same.
        assert!(assembled().contains(".status-pill"));
        assert!(assembled().contains(".status-pill.visible"));
        assert!(assembled().contains("statusPill"));
        assert!(assembled().contains("statusPill.flash"));
        assert!(assembled().contains("ev.kind === 'status'"));
    }

    #[test]
    fn faceplate_preset_bar_wired() {
        // 0049: preset bar replaces the empty placeholder div. Markup
        // carries the current-name slot, prev/next walker buttons, the
        // Browse toggle, the Save As button, and the in-bar status chip.
        for id in [
            "id=\"pbar-prev\"",
            "id=\"pbar-name\"",
            "id=\"pbar-next\"",
            "id=\"pbar-browse\"",
            "id=\"pbar-save\"",
            "id=\"pbar-status\"",
        ] {
            assert!(assembled().contains(id), "preset bar missing {id}");
        }
        // JS bridge: prev/next post `step_preset` with signed delta; Save
        // As funnels through the 0048 popup then posts `save_preset` with
        // `folder: null`; preset_loaded sets the name.
        assert!(assembled().contains("send.stepPreset(-1)"));
        assert!(assembled().contains("send.stepPreset(1)"));
        assert!(assembled().contains("send.savePreset("));
        // Save As funnels through the in-WebView modal (name field +
        // folder dropdown) rather than going straight through the native
        // popup. The modal posts `save_preset` directly; presetBar just
        // opens it. `browserPanel.getSaveFolder()` is still exposed for
        // other call sites but no longer the Save As path.
        assert!(assembled().contains("browserPanel.openSaveAs"));
        assert!(assembled().contains("ev.kind === 'preset_loaded'"));
        assert!(assembled().contains("presetBar.setName"));
        // 0050: Browse toggles the panel itself via `browserPanel.setOpen`;
        // the `onOpenChange` callback drives the bar's active-class mirror.
        // (0081 dropped the dead `window.vxn._browserOpen` write.)
        assert!(assembled().contains("browserPanel.setOpen"));
    }

    #[test]
    fn faceplate_browser_mutation_flows_wired() {
        // 0051: every mutation op the controller exposes has a JS post
        // site inside the browser panel. The IIFE wires:
        // - Rename: posts `rename_preset` / `rename_folder` via the
        //   text-input popup.
        // - Delete: modal confirm posts `delete_preset` / `delete_folder`
        //   (the Vizia version's two-click row-armed pattern was scrapped
        //   here — the right-click menu obscured the row text).
        // - Move to: submenu posts `move_preset` with the destination
        //   folder (or null for user root).
        // - New Folder: "+ New" button on the user header posts
        //   `new_folder` after the popup commits.
        assert!(assembled().contains("send.renamePreset("));
        assert!(assembled().contains("send.renameFolder("));
        assert!(assembled().contains("send.deletePreset("));
        assert!(assembled().contains("send.deleteFolder("));
        assert!(assembled().contains("send.movePreset("));
        assert!(assembled().contains("send.newFolder("));
        // Modal confirm primitive present; ESC tears down modal → menu →
        // panel in that order (one level per press).
        assert!(assembled().contains("openDeleteConfirm"));
        assert!(assembled().contains(".browser-modal"));
        assert!(assembled().contains(".browser-modal-backdrop"));
        assert!(assembled().contains("if (modalEl)"));
        // Right-click hooks on both row types (factory rows must not
        // attach one — the JS gates by selectedFolder.kind / key.kind).
        assert!(assembled().contains("'contextmenu'"));
        // Move-to submenu helper present; mirrors `vxn_ui_vizia::move_targets`.
        // 0077 lifted `moveTargets` to module scope (so the Node test
        // suite can import it pure) and added `corpus` as an explicit arg.
        assert!(assembled().contains("moveTargets(currentName, corpus)"));
        assert!(assembled().contains(".browser-menu"));
        assert!(assembled().contains(".browser-submenu"));
        assert!(assembled().contains(".browser-new-folder"));
    }

    #[test]
    fn faceplate_save_as_modal_wired() {
        // Save As modal hosts a name field (captured via the native
        // popup for spacebar-safe entry) + a folder dropdown over user
        // folders. The modal posts `save_preset { name, folder }`.
        assert!(assembled().contains("openSaveAsModal"));
        // The name field reuses `promptText` so Space and friends still
        // route through the native NSWindow on macOS.
        assert!(assembled().contains("window.vxn.promptText('Preset name'"));
        // Folder choices come from a `<select>` populated from the corpus.
        assert!(assembled().contains("folderOptions"));
        assert!(assembled().contains(".save-as-select"));
        // Modal anchors over the faceplate, not the browser panel — so
        // Save As works whether the browser is open or not.
        assert!(assembled().contains("getElementById('faceplate').appendChild(wrap)"));
        // Save button is disabled until the name field is non-empty
        // (gateOk toggles the disabled attribute directly).
        assert!(assembled().contains("gateOk"));
        assert!(assembled().contains(".browser-modal-btn:disabled"));
    }

    #[test]
    fn faceplate_browser_search_is_cross_folder() {
        // Non-empty query: the right pane switches to flat search results
        // covering the whole corpus (factory + user) instead of filtering
        // within the selected folder only.
        assert!(assembled().contains("collectSearchHits"));
        assert!(assembled().contains("'Factory · '"));
        assert!(assembled().contains("'User · '"));
        // Search-mode row carries name + muted origin label.
        assert!(assembled().contains(".browser-row.search-row"));
        assert!(assembled().contains(".browser-row-origin"));
    }

    #[test]
    fn faceplate_browser_panel_wired() {
        // 0050: floating two-pane browser. Markup carries the search input,
        // the folders + presets panes, and the click-outside backdrop. The
        // panel and its backdrop start hidden (`hidden` attribute, toggled
        // by `setOpen`).
        for needle in [
            r#"id="browser-panel""#,
            r#"id="browser-backdrop""#,
            r#"id="browser-folders""#,
            r#"id="browser-presets""#,
            r#"id="browser-search-input""#,
            r#"id="browser-search-clear""#,
        ] {
            assert!(assembled().contains(needle), "browser panel missing {needle}");
        }
        // JS module + Rust→JS corpus channel.
        assert!(assembled().contains("const browserPanel"));
        assert!(assembled().contains("window.__vxn.applyPresetCorpus"));
        // Bootstrap stub funnels the first snapshot into `_earlyPresetCorpus`
        // so any corpus push that races init() is replayed.
        assert!(assembled().contains("_earlyPresetCorpus"));
        // Click handlers: folder click rerenders presets, preset click posts
        // load_factory or load_user (browser panel routes by folder kind).
        assert!(assembled().contains("send.loadFactory("));
        assert!(assembled().contains("send.loadUser("));
        // Dismissal: ESC + outside-click backdrop both close the panel.
        assert!(assembled().contains("e.key !== 'Escape'"));
        assert!(assembled().contains("backdropEl.addEventListener('click'"));
        // Highlight: preset_loaded fans `source` into the panel's
        // currently-loaded marker.
        assert!(assembled().contains("browserPanel.setCurrentSource"));
        // Section headers match the Vizia browser's labels.
        assert!(assembled().contains("'FACTORY'"));
        assert!(assembled().contains("'USER'"));
    }

    #[test]
    fn corpus_snapshot_groups_and_sorts() {
        use vxn_app::{UserFolderEntry, UserPresetEntry};
        let factory = vec![
            PresetMeta { name: "zeta".into(), category: Some("Lead".into()), ..Default::default() },
            PresetMeta { name: "Alpha".into(), category: Some("Lead".into()), ..Default::default() },
            PresetMeta { name: "Pad-A".into(), category: Some("Pad".into()), ..Default::default() },
            PresetMeta { name: "loose".into(), category: None, ..Default::default() },
        ];
        let user = vec![
            UserFolderEntry {
                name: Some("Bass".into()),
                presets: vec![
                    UserPresetEntry {
                        path: PathBuf::from("/u/Bass/B.preset"),
                        meta: PresetMeta { name: "B".into(), ..Default::default() },
                        folder: Some("Bass".into()),
                    },
                    UserPresetEntry {
                        path: PathBuf::from("/u/Bass/a.preset"),
                        meta: PresetMeta { name: "a".into(), ..Default::default() },
                        folder: Some("Bass".into()),
                    },
                ],
            },
            UserFolderEntry {
                name: None,
                presets: vec![UserPresetEntry {
                    path: PathBuf::from("/u/loose.preset"),
                    meta: PresetMeta { name: "loose".into(), ..Default::default() },
                    folder: None,
                }],
            },
            UserFolderEntry {
                name: Some("Aux".into()),
                presets: vec![],
            },
        ];
        let corpus = vxn_app::PresetCorpus { factory, user };
        let s = corpus_snapshot_json(&corpus);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        // Factory groups sorted by category (case-insensitive), each
        // group's presets sorted by name.
        let fac = v["factory"].as_array().unwrap();
        let cats: Vec<&str> = fac.iter().map(|g| g["category"].as_str().unwrap()).collect();
        assert_eq!(cats, vec!["Lead", "Pad", UNCATEGORIZED]);
        let lead = fac[0]["presets"].as_array().unwrap();
        assert_eq!(lead[0]["name"], "Alpha");
        assert_eq!(lead[1]["name"], "zeta");
        // Factory index points back into the original corpus order
        // (so `load_factory { index }` works).
        assert_eq!(lead[0]["index"], 1);
        assert_eq!(lead[1]["index"], 0);
        // Uncategorised group carries the orphan factory preset.
        assert_eq!(fac[2]["presets"][0]["name"], "loose");

        // User folders: root (None) first, then sorted named folders;
        // each folder's presets sorted by name.
        let user = v["user"].as_array().unwrap();
        assert!(user[0]["name"].is_null(), "root folder must come first");
        assert_eq!(user[1]["name"], "Aux");
        assert_eq!(user[2]["name"], "Bass");
        let bass = user[2]["presets"].as_array().unwrap();
        assert_eq!(bass[0]["name"], "a");
        assert_eq!(bass[1]["name"], "B");
        assert_eq!(bass[0]["path"], "/u/Bass/a.preset");
    }

    // ── Row 1 + Row 2 control mount points (0041, 0041a, 0042, 0043) ────

    #[test]
    fn row1_osc_mixer_panels_have_expected_mounts() {
        // Wave + four faders per Osc panel; four level faders + one Col
        // switch on the Mixer; LFO 1 (Shape/Rate/Delay/Fade up top, Sync +
        // Free toggles in the strip) and LFO 2 (Shape/Rate, Sync in the
        // strip). Param names are descriptor `name`s so a `PatchParam` enum
        // reorder doesn't break the HTML.
        for (kind, name, label) in [
            // LFO 1
            ("wave",   "lfo_shape",       "Shape"),
            ("fader",  "lfo_rate",        "Rate"),
            ("fader",  "lfo1_delay_time", "Delay"),
            ("fader",  "lfo1_fade",       "Fade"),
            ("switch", "lfo_sync",        "Sync"),
            ("switch", "lfo1_free_run",   "Free"),
            // LFO 2
            ("wave",   "lfo2_shape", "Shape"),
            ("fader",  "lfo2_rate",  "Rate"),
            ("switch", "lfo2_sync",  "Sync"),
            // Osc 1
            ("wave",  "osc1_wave",   "Wave"),
            ("fader", "osc1_octave", "Oct"),
            ("fader", "osc1_coarse", "Semi"),
            ("fader", "osc1_fine",   "Fine"),
            ("fader", "osc1_pw",     "PW"),
            // Osc 2
            ("wave",  "osc2_wave",   "Wave"),
            ("fader", "osc2_octave", "Oct"),
            ("fader", "osc2_coarse", "Semi"),
            ("fader", "osc2_fine",   "Fine"),
            ("fader", "osc2_pw",     "PW"),
            // Mixer
            ("fader",  "osc1_level",  "Osc1"),
            ("fader",  "osc2_level",  "Osc2"),
            ("fader",  "sub_level",   "Sub"),
            ("fader",  "noise_level", "Noise"),
            ("switch", "noise_color", "Col"),
        ] {
            let marker = format!(
                r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
            );
            assert!(
                assembled().contains(&marker),
                "Row 1 mount point missing: {marker}",
            );
        }
    }

    #[test]
    fn row2_env_filter_panels_have_expected_mounts() {
        // Env 1/2: ADSR faders + Shape switch in the bottom strip (Vizia
        // maps the 2-variant Lin/Exp enum to a switch via `in_bottom_strip`).
        // VCA: AmpLfoSrc dropdown + Depth fader; AmpEnvBypass in strip.
        // Filter: HPF/Cutoff/Reso/Drive faders + Mode dropdown; Slope (12/24
        // dB enum) and KeyTrk (bool) ride the strip. Filter Mod: four fixed
        // depths into cutoff (E006), no source selectors. Names match the
        // `ParamDesc.name` fields so a `PatchParam` enum reorder doesn't
        // break the HTML.
        for (kind, name, label) in [
            // Env 1
            ("fader",  "env1_attack",  "A"),
            ("fader",  "env1_decay",   "D"),
            ("fader",  "env1_sustain", "S"),
            ("fader",  "env1_release", "R"),
            ("switch", "env1_shape",   "Shape"),
            // Env 2
            ("fader",  "env2_attack",  "A"),
            ("fader",  "env2_decay",   "D"),
            ("fader",  "env2_sustain", "S"),
            ("fader",  "env2_release", "R"),
            ("switch", "env2_shape",   "Shape"),
            // VCA
            ("buttongroup", "amp_lfo_src",    "LFO"),
            ("fader",       "amp_lfo_depth",  "Depth"),
            ("switch",      "amp_env_bypass", "Gate"),
            // Filter
            ("fader",       "hpf_cutoff",       "HPF"),
            ("fader",       "cutoff",           "Cutoff"),
            ("fader",       "resonance",        "Reso"),
            ("fader",       "drive",            "Drive"),
            ("buttongroup", "filter_mode",      "Mode"),
            ("switch",      "filter_slope",     "Slope"),
            ("switch",      "filter_key_track", "KeyTrk"),
            // Filter Mod
            ("fader", "vel_cutoff_depth",  "Vel"),
            ("fader", "cutoff_lfo1_depth", "LFO1"),
            ("fader", "cutoff_lfo2_depth", "LFO2"),
            ("fader", "cutoff_env_depth",  "Env1"),
        ] {
            let marker = format!(
                r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
            );
            assert!(
                assembled().contains(&marker),
                "Row 2 mount point missing: {marker}",
            );
        }
    }

    #[test]
    fn row3_mod_route_panels_have_expected_mounts() {
        // 0044: Pitch Mod / PWM Mod each carry two route columns (depth
        // fader + source buttongroup). Cross Mod is the wide custom panel
        // (Type buttongroup + Amount fader, Src buttongroup + Mod fader).
        // Mod Wheel = four cutoff/pwm/reso/pitch destination faders. Bend
        // is the single-fader pinned-width panel. Names match the
        // `ParamDesc.name` fields so a `PatchParam` enum reorder doesn't
        // break the HTML.
        for (kind, name, label) in [
            // Pitch Mod
            ("buttongroup", "pitch_lfo_src",      "LFO"),
            ("switch",      "pitch_lfo_mod_only", "Mod"),
            ("buttongroup", "pitch_env_src",      "Env"),
            ("switch",      "pitch_env_mod_only", "Mod"),
            // PWM Mod
            ("buttongroup", "pwm_lfo_src",   "LFO"),
            ("buttongroup", "pwm_env_src",   "Env"),
            // Cross Mod
            ("buttongroup", "cross_mod_type",       "Type"),
            ("fader",       "cross_mod_amount",     "Amt"),
            // Mod Wheel
            ("fader", "mod_wheel_pwm",        "PWM"),
            ("fader", "mod_wheel_cutoff",     "Cutoff"),
            ("fader", "mod_wheel_reso",       "Reso"),
            ("fader", "mod_wheel_cross_mod_sweep", "X-Mod"),
            // Bend
            ("fader", "pitch_wheel_depth", "Range"),
        ] {
            let marker = format!(
                r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
            );
            assert!(
                assembled().contains(&marker),
                "Row 3 mount point missing: {marker}",
            );
        }
        // Pitch Mod / PWM Mod depth faders carry `data-no-label` — the
        // route header (LFO / Env) is the only column label, matching the
        // source buttongroup beside them.
        for name in [
            "pitch_lfo_depth",
            "pitch_env_depth",
            "pwm_lfo_depth",
            "pwm_env_depth",
        ] {
            let marker = format!(
                r#"data-control="fader" data-param="{name}" data-dim-when-src-off="#,
            );
            assert!(
                assembled().contains(&marker),
                "Pitch Mod depth fader missing: {marker}",
            );
            assert!(
                !assembled().contains(&format!(r#"data-param="{name}" data-label="#)),
                "Pitch Mod depth fader {name} should not carry data-label",
            );
        }
    }

    #[test]
    fn row4_voice_master_fx_panels_have_expected_mounts() {
        // 0045: Voice = AssignMode (with display-order 0,3,1,2 → Poly,
        // Twin, Unison, Solo) + Detune-Legato composite + Glide fader.
        // Master = Tune/Volume faders, Limit switch (header-less, like
        // vizia's `limiter_cell`), Oversample buttongroup. Chorus + Delay
        // each carry a header-switch (chorus_on / delay_on) plus their
        // body faders; Delay's Sync + Ping-Pong drop to the strip per
        // `vxn_ui_vizia::in_bottom_strip`. Names = descriptor names.
        for (kind, name, label) in [
            // Voice
            ("buttongroup",   "assign_mode",     "Assign"),
            ("detune-legato", "unison_detune",   "Detune"),
            ("fader",         "portamento_time", "Glide"),
            // Master
            ("fader",  "master_tune",   "Tune"),
            ("fader",  "master_volume", "Volume"),
            ("switch", "oversample",    "OvSmp"),
            ("switch", "limiter_on",    "Limit"),
            // Chorus
            ("header-switch", "chorus_on",    ""),
            ("fader",         "chorus_rate",  "Rate"),
            ("fader",         "chorus_depth", "Depth"),
            ("fader",         "chorus_mix",   "Mix"),
            // Delay
            ("header-switch", "delay_on",       ""),
            ("fader",         "delay_time",     "Time"),
            ("fader",         "delay_feedback", "FB"),
            ("fader",         "delay_mix",      "Mix"),
            ("switch",        "delay_sync",     "Sync"),
            ("switch",        "delay_pingpong", "P-Pong"),
            // Reverb (E012 / 0058)
            ("header-switch", "reverb_on",    ""),
            ("buttongroup",   "reverb_type",  "Type"),
            ("fader",         "reverb_depth", "Depth"),
            ("fader",         "reverb_mix",   "Mix"),
        ] {
            // Header-switch slots carry no `data-label` attribute; assert
            // on the kind+name pair instead.
            let needle = if kind == "header-switch" {
                format!(r#"data-control="{kind}" data-param="{name}""#)
            } else {
                format!(
                    r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
                )
            };
            assert!(
                assembled().contains(&needle),
                "Row 4 mount point missing: {needle}",
            );
        }
        // Voice's AssignMode buttongroup carries the display permutation
        // (descriptor order = Poly/Unison/Solo/Twin → display order =
        // Poly/Twin/Unison/Solo). If the descriptor order changes, this
        // attribute changes alongside; the test guards the wiring.
        assert!(
            assembled().contains(r#"data-param="assign_mode" data-label="Assign" data-order="0,3,1,2""#),
            "AssignMode missing display-order remap",
        );
        // Detune-Legato carries its two extra param-name dependencies so
        // a layer rebind can re-resolve both alongside the primary param.
        assert!(
            assembled().contains(r#"data-legato-param="legato""#),
            "Detune-Legato missing data-legato-param",
        );
        assert!(
            assembled().contains(r#"data-mode-param="assign_mode""#),
            "Detune-Legato missing data-mode-param",
        );
    }

    #[test]
    fn control_tallies_match_all_rows() {
        // Global mount-point tally — catches duplicate mounts / typos that
        // accept a missing `<div>` somewhere else. Counts each control
        // kind across all four rows.
        //
        // Faders:
        //   Row 1: LFO1 3 (Rate/Delay/Fade), LFO2 1 (Rate), Osc1 4, Osc2 4, Mixer 3 = 15
        //   Row 2: Env1 4, Env2 4, VCA 1, Filter 4, FilterMod 4              = 17
        //   Row 3: PitchMod 2, PwmMod 2, CrossMod 1, ModWheel 4, Bend 1      = 10
        //   Row 4: Voice 1 (Glide), Master 2, Chorus 3, Delay 3,
        //          Reverb 2 (Depth/Mix)                                      = 11
        //   Total = 53.
        // Waves: 4 (LFO 1/2 Shape, Osc 1/2 Wave).
        // Switches:
        //   Row 1: 4 (LfoSync, Lfo2Sync, Lfo1FreeRun, NoiseColor)
        //   Row 2: 5 (Env1Shape, Env2Shape, Gate, Slope, KeyTrk)
        //   Row 3: 2 (PitchLfoModOnly, PitchEnvModOnly)
        //   Row 4: 4 (Oversample as multi-toggle row, LimiterOn,
        //            DelaySync, DelayPingPong)
        //   Total = 15.
        // Button groups:
        //   Row 2: 2 (AmpLfoSrc, FilterMode)
        //   Row 3: 5 (Pitch/PWM LFO+Env sources, CrossModType)
        //   Row 4: 2 (AssignMode, ReverbType) — Oversample renders as a
        //     horizontal switch row at the bottom of Master, not a vertical
        //     buttongroup column.
        //   Total = 9.
        // Header switches: 3 (Chorus, Delay, Reverb).
        // Detune-Legato composite: 1 (Voice).
        assert_eq!(
            assembled().matches(r#"data-control="fader""#).count(),
            54,
            "expected 54 fader cells across all four rows",
        );
        assert_eq!(
            assembled().matches(r#"data-control="wave""#).count(),
            4,
            "expected 4 wave cells (LFO 1, LFO 2, Osc 1, Osc 2)",
        );
        assert_eq!(
            assembled().matches(r#"data-control="switch""#).count(),
            15,
            "expected 15 switch cells (Row 1 + Row 2 + Row 3 + Row 4)",
        );
        assert_eq!(
            assembled().matches(r#"data-control="buttongroup""#).count(),
            9,
            "expected 9 buttongroup cells (Row 2 + Row 3 + Row 4)",
        );
        assert_eq!(
            assembled().matches(r#"data-control="dropdown""#).count(),
            0,
            "no dropdown cells expected (all enums fit ButtonGroup)",
        );
        assert_eq!(
            assembled().matches(r#"data-control="header-switch""#).count(),
            3,
            "expected 3 header-switch cells (Chorus, Delay, Reverb)",
        );
        assert_eq!(
            assembled().matches(r#"data-control="detune-legato""#).count(),
            1,
            "expected 1 detune-legato composite (Voice)",
        );
    }

    #[test]
    fn mod_route_dim_rules_present() {
        // 0044: Cross Mod's Amount fader dims unless Type = FM (matches
        // vxn_ui_vizia::xmod_pair's FM-only enable); Mod fader dims when
        // Src = Off. Pitch/PWM Mod follow the same convention — the
        // *depth fader* dims when its source reads Off, not the source
        // selector itself (selector stays bright so a routed-Off path is
        // still readable / clickable).
        assert!(
            assembled().contains(r#"data-dim-unless-fm="cross_mod_type""#),
            "Cross Mod Amount missing data-dim-unless-fm wiring",
        );
        for (depth, src) in [
            ("pitch_lfo_depth", "pitch_lfo_src"),
            ("pitch_env_depth", "pitch_env_src"),
            ("pwm_lfo_depth",   "pwm_lfo_src"),
            ("pwm_env_depth",   "pwm_env_src"),
            // VCA's Amp LFO Depth follows the same rule (Off / LFO 1 /
            // LFO 2 source → fader dims on Off).
            ("amp_lfo_depth",   "amp_lfo_src"),
        ] {
            // Pitch Mod / PWM Mod depth faders dropped their `data-label`
            // (route header names the column), so the marker between
            // data-param and data-dim-when-src-off differs from the others.
            let marker = if depth.starts_with("pitch_") || depth.starts_with("pwm_") {
                format!(
                    r#"data-param="{depth}" data-dim-when-src-off="{src}""#,
                )
            } else {
                format!(
                    r#"data-param="{depth}" data-label="Depth" data-dim-when-src-off="{src}""#,
                )
            };
            assert!(
                assembled().contains(&marker),
                "route depth {depth} missing dim-when-src-off=\"{src}\"",
            );
        }
        // Route-column source selectors must NOT carry the self-dim
        // marker — selectors stay bright; only the paired fader dims.
        assert_eq!(
            assembled().matches("data-dim-when-zero").count(),
            0,
            "route-col source selectors should no longer self-dim",
        );
        // JS dispatch wires the generic dim rule into ParamChanged.
        assert!(assembled().contains("applyDimRulesFor("));
        assert!(assembled().contains("collectDimRuleSpecs"));
    }

    #[test]
    fn edit_layer_rebind_wired() {
        // 0045: EditLayerChanged ViewEvent dispatch + layer-rebind logic
        // present. The actual rebind walks LAYERED_CELLS and re-resolves
        // each per-patch name → id via paramIdByNameAtLayer using the
        // patchCount splice.
        assert!(assembled().contains("edit_layer_changed"));
        assert!(assembled().contains("rebindAllForLayer"));
        assert!(assembled().contains("paramIdByNameAtLayer"));
        // Placeholder lives in bridge.js pre-splice — assembled() has already
        // replaced it, so check the raw bridge module.
        assert!(BRIDGE_JS.contains("__PATCH_COUNT__"));
        // The splice replaces the placeholder at render time.
        let html = build_faceplate_html();
        assert!(!html.contains("__PATCH_COUNT__"), "patchCount placeholder must be replaced");
        assert!(
            html.contains(&format!("patchCount: {}", vxn_app::PATCH_COUNT)),
            "patchCount splice value missing from rendered html",
        );
    }

    #[test]
    fn header_switch_primitive_wired() {
        // 0045: Chorus + Delay carry a header-switch in
        // `.panel-header-toggle-slot`; CSS provides the active palette.
        assert!(assembled().contains("makeHeaderSwitch"));
        assert!(assembled().contains(".panel-header-switch"));
        assert!(assembled().contains(".panel-header-switch.active"));
    }

    #[test]
    fn edit_layer_changed_serializes() {
        // The web crate's view_event_to_json must encode the new variant
        // for the JS dispatcher to ever see it.
        let s = view_event_to_json(&ViewEvent::EditLayerChanged { layer: Layer::Lower });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "edit_layer_changed");
        assert_eq!(v["layer"], "lower");
    }

    #[test]
    fn split_point_changed_serializes() {
        // 0053: HTML Keys panel needs the controller's split-point
        // re-broadcast (preset / state-load / EditorReady) to reseed its
        // slider, since the page has no idle-poll loop the vizia editor
        // uses to read `SharedParams::split_point()` directly.
        let s = view_event_to_json(&ViewEvent::SplitPointChanged { note: 72 });
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "split_point_changed");
        assert_eq!(v["note"], 72);
    }

    #[test]
    fn keys_panel_wired() {
        // 0053: Keys panel — mode/edit toggles, split slider with
        // C0..C7 range, note-name readout, Reset button. UiEvent posts:
        //   - set_key_mode (Whole / Dual / Split row)
        //   - set_edit_layer (Upper / Lower row, hidden in Whole)
        //   - set_split_point (slider, visible only in Split)
        //   - reset_layer (Reset button — both layers in Whole, the
        //     active layer otherwise)
        // ViewEvent dispatch:
        //   - key_mode_changed → keysPanel.setMode
        //   - edit_layer_changed → keysPanel.setLayer (in addition to
        //     the per-patch rebind)
        //   - split_point_changed → keysPanel.setSplit
        assert!(assembled().contains("const keysPanel = "));
        assert!(assembled().contains("send.setKeyMode("));
        assert!(assembled().contains("send.setEditLayer("));
        assert!(assembled().contains("send.setSplitPoint("));
        assert!(assembled().contains("send.resetLayer("));
        assert!(assembled().contains("ev.kind === 'key_mode_changed'"));
        assert!(assembled().contains("ev.kind === 'split_point_changed'"));
        assert!(assembled().contains("keysPanel.setMode"));
        assert!(assembled().contains("keysPanel.setLayer"));
        assert!(assembled().contains("keysPanel.setSplit"));
        // Note-name readout: covers a C0..C7 span, matches the vizia
        // editor's `note_name`.
        assert!(assembled().contains("function keysNoteName("));
        assert!(assembled().contains("KEYS_SPLIT_MIN = 12"));
        assert!(assembled().contains("KEYS_SPLIT_MAX = 96"));
        // Default split: matches DEFAULT_SPLIT_POINT (C4) so a
        // double-click reset lands on the same plain value the vizia
        // editor's `on_double_click` posts.
        assert_eq!(vxn_app::DEFAULT_SPLIT_POINT, 60);
        assert!(assembled().contains("KEYS_DEFAULT_SPLIT = 60"));
        // The slot reserved by 0040 now carries real markup, not a
        // bare placeholder. The vizia overlay note is gone.
        assert!(assembled().contains("data-name=\"Keys\""));
        assert!(!assembled().contains("still rendered by vizia"));
    }

    #[test]
    fn filter_mode_notch_dims_slope_strip() {
        // 0043: Filter Mode = Notch dims the Slope strip cell (DSP no-op,
        // see vxn-dsp/src/ota_ladder.rs). Test guards the wiring rather
        // than the runtime toggle:
        //   - CSS targets both `.ctl.dimmed` and `.ctl-strip.dimmed` (slope
        //     lives in the strip).
        //   - JS resolves `filter_mode` + `filter_slope` and looks up the
        //     Notch variant by label (so a `FILTER_MODE_LABELS` reorder
        //     doesn't desync).
        //   - The dispatch branch keys on `FILTER_MODE_ID`.
        // Asserting on the assembled HTML keeps the test substring-based —
        // the existing Free-run dim has the same shape.
        assert!(
            assembled().contains(".ctl-strip.dimmed"),
            "missing strip dim selector (slope dim relies on it)",
        );
        assert!(assembled().contains("BUILTIN_DIM_SPECS"));
        assert!(assembled().contains("'filter-notch'"));
        assert!(assembled().contains("variantIdx('filter_mode', 'Notch'"));
        assert!(assembled().contains("data-param=\"filter_slope\""));
        assert!(assembled().contains("applyDimRulesFor("));
    }

    #[test]
    fn faceplate_has_subdivisions_json_placeholder() {
        // SUBDIVISIONS table is spliced as a JSON array of labels; the LFO
        // rate fader's displayOverride indexes it when sync is on (0042).
        assert!(BRIDGE_JS.contains("__SUBDIVISIONS_JSON__"));
        let html = build_faceplate_html();
        assert!(!html.contains("__SUBDIVISIONS_JSON__"));
        // Sanity check: array matches the Rust table 1:1.
        let json = build_subdivisions_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("subdivisions JSON");
        let arr = v.as_array().expect("array root");
        assert_eq!(arr.len(), vxn_app::sync::SUBDIVISIONS.len());
        for (i, s) in vxn_app::sync::SUBDIVISIONS.iter().enumerate() {
            assert_eq!(arr[i], s.label);
        }
    }

    #[test]
    fn faceplate_has_params_json_placeholder() {
        // The template carries `__PARAMS_JSON__` for runtime descriptor
        // injection; build_faceplate_html() splices it.
        assert!(BRIDGE_JS.contains("__PARAMS_JSON__"));
        let html = build_faceplate_html();
        assert!(!html.contains("__PARAMS_JSON__"), "placeholder must be replaced");
        // Page references the bridge property; sanity check the rendered HTML
        // still contains the field literal.
        assert!(html.contains("params:"));
        // Splice the params JSON directly and prove its shape.
        let json = build_params_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("descriptor JSON");
        // Upper Osc1Wave is CLAP id 0.
        assert_eq!(v["0"]["name"], "osc1_wave");
        assert_eq!(v["0"]["kind"], "enum");
        assert_eq!(v["0"]["variants"][0], "Sine");
    }

    #[test]
    fn descriptor_json_covers_every_kind() {
        // Walk every descriptor and confirm `kind` is one of the four expected
        // discriminants. Catches a future ParamKind variant slipping through
        // without a JSON-side handler.
        let v: serde_json::Value = serde_json::from_str(&build_params_json()).expect("params JSON");
        let obj = v.as_object().expect("object root");
        let mut seen_float = false;
        let mut seen_int = false;
        let mut seen_bool = false;
        let mut seen_enum = false;
        for (_id, desc) in obj {
            let kind = desc["kind"].as_str().unwrap_or("");
            assert!(
                matches!(kind, "float" | "int" | "bool" | "enum"),
                "unknown kind \"{kind}\" in {desc}",
            );
            match kind {
                "float" => seen_float = true,
                "int" => seen_int = true,
                "bool" => seen_bool = true,
                "enum" => seen_enum = true,
                _ => {}
            }
        }
        assert!(seen_float && seen_int && seen_bool && seen_enum);
    }

    #[test]
    fn faceplate_browser_drag_drop_wired() {
        // 0052: HTML5 DnD. Drag source = user-side preset rows (folder
        // view + search view). Drop target = user folder rows (left
        // pane). Factory rows have no DnD listeners on either side —
        // gated by `selectedFolder.kind === 'user'` /
        // `key.kind === 'user'`.
        //
        // Wire shape:
        //   - `row.draggable = true` + `dragstart` sets
        //     `dataTransfer.setData('vxn/preset', path)` (custom MIME
        //     guards against external dropzones receiving a preset path)
        //     plus module-level `dragSourcePath` / `dragSourceFolder`
        //     (read during `dragover` because `dataTransfer.getData` is
        //     not callable then).
        //   - Drop target preventDefaults `dragover` only when source is
        //     a vxn preset AND the target is not the source folder; the
        //     source folder shows `.drag-blocked` instead.
        //   - Drop posts `op: 'move_preset'` with the destination
        //     folder name (or null for the virtual user root).
        // The Move-to ▸ submenu (0051) shares the `move_preset` op
        // string, so this test additionally asserts the DnD-specific
        // bridge surface (drag listeners, MIME, drop CSS).
        assert!(assembled().contains("'vxn/preset'"));
        assert!(assembled().contains("wirePresetDragSource"));
        assert!(assembled().contains("'dragstart'"));
        assert!(assembled().contains("'dragover'"));
        assert!(assembled().contains("'dragleave'"));
        assert!(assembled().contains("'dragend'"));
        // The drop handler shares the `move_preset` op with the Move-to
        // submenu; the DnD-specific path passes `dragSourcePath` rather
        // than the menu's `target.path`. The `dragSourcePath` identifier
        // is used by both the dragstart write and the drop read — its
        // mere presence proves the bridge is wired through.
        assert!(assembled().contains("send.movePreset(dragSourcePath"));
        // Drop-target gating: factory rows must not get listeners. The
        // `appendFolderRow` gate keys on `key.kind === 'user'`; assert
        // the source folder no-op branch is present (key.name ===
        // dragSourceFolder).
        assert!(assembled().contains("key.name === dragSourceFolder"));
        // CSS for drop-target highlight + source-folder block + drag-
        // source dimming. `.drag-over` is the live drop highlight;
        // `.drag-blocked` shows the source folder mid-drag.
        assert!(assembled().contains(".browser-row.drag-over"));
        assert!(assembled().contains(".browser-row.drag-blocked"));
        assert!(assembled().contains(".browser-row.dragging"));
        // Follow-path plumbing: PresetCorpusChanged carries an
        // Option<PathBuf>; non-null means reselect the folder and
        // scroll the moved row into view. Dispatcher branch + module
        // method both present.
        assert!(assembled().contains("ev.kind === 'preset_corpus_changed'"));
        assert!(assembled().contains("browserPanel.followPath"));
        assert!(assembled().contains("function followPath("));
        // Rendered rows tag themselves with `data-path` so followPath
        // can locate the moved row via a CSS attribute selector.
        assert!(assembled().contains("r.dataset.path = p.path"));
        assert!(assembled().contains("r.dataset.path = h.source.path"));
    }

    // ── JS suite gate (E015 / 0078) ─────────────────────────────────────
    //
    // The Vitest + jsdom suite under `assets/__tests__/` is the
    // behavioural net for the four faceplate JS modules. We shell `npm
    // test` from a `#[test]` so `cargo test -p vxn-ui-web` is still the
    // single command a contributor runs locally. The env-gate keeps the
    // default `cargo test` Rust-only (no Node dep) — set `VXN_JS_TESTS=1`
    // to opt in. CI (when one lands) sets the var so the gate is real.
    #[test]
    fn js_suite_passes() {
        if std::env::var("VXN_JS_TESTS").is_err() {
            // No-op skip rather than `#[ignore]`: a build-script `cfg`
            // would work, but a runtime check keeps the gate one place
            // and matches the ticket-spec'd alternative.
            eprintln!(
                "VXN_JS_TESTS unset; skipping JS suite. \
                 Run `VXN_JS_TESTS=1 cargo test -p vxn-ui-web` to enable."
            );
            return;
        }
        let status = std::process::Command::new("npm")
            .args(["test", "--silent"])
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"))
            .status()
            .expect("npm not found — install Node 20+ or unset VXN_JS_TESTS");
        assert!(status.success(), "JS suite failed under `npm test`");
    }
}
