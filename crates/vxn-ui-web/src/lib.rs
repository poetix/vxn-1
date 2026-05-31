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

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use raw_window_handle::{
    HandleError, HasWindowHandle, RawWindowHandle, WindowHandle as RwhWindowHandle,
};
use vxn_app::{
    ControllerHandle, EditorBackend, KeyMode, Layer, PATCH_COUNT, ParamDesc, ParamId, ParamKind,
    PresetSource, TOTAL_PARAMS, UiEvent, ViewEvent, desc_for_clap_id,
};
use wry::{Rect, WebView, WebViewBuilder};
use wry::dpi::{LogicalPosition, LogicalSize};

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
}

impl EditorHandle {
    /// Buffer one [`ViewEvent`] for the current tick. Flushed by
    /// [`Self::flush_view_events`]; nothing crosses into JS until then.
    pub fn push_view_event(&self, event: ViewEvent) {
        self.buf.borrow_mut().push(event);
    }

    /// Drain the batch into one `__vxn.applyViewEvents` call (or several, if
    /// the JSON exceeds [`MAX_BATCH_BYTES`]). `ParamChanged` events dedupe by
    /// id within the batch — only the latest value per param survives, which
    /// caps the bridge at one update per param per tick regardless of how
    /// many automation writes the audio thread did between ticks.
    pub fn flush_view_events(&self) {
        let events = std::mem::take(&mut *self.buf.borrow_mut());
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

    fn open(parent: Self::ParentWindow, ctrl: ControllerHandle) -> Self::Handle {
        open_editor(parent, ctrl)
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
pub fn open_editor(parent: *mut c_void, ctrl: ControllerHandle) -> EditorHandle {
    let parent = ParentWindow { raw: build_raw(parent) };
    let html = build_faceplate_html();
    let webview = WebViewBuilder::new_as_child(&parent)
        .with_html(html)
        .with_bounds(Rect {
            position: LogicalPosition::new(0i32, 0i32).into(),
            size: LogicalSize::new(EDITOR_WIDTH, EDITOR_HEIGHT).into(),
        })
        .with_ipc_handler(move |req| {
            if let Some(ev) = parse_ui_event(req.body()) {
                let _ = ctrl.post(ev);
            }
        })
        .build()
        .expect("wry WebView build failed");
    EditorHandle { webview, buf: RefCell::new(Vec::new()) }
}

/// Splice the runtime param-descriptor JSON into the faceplate template. The
/// page reads it as `window.vxn.params = {...}`, a CLAP-id-keyed map of
/// `{name, label, kind, min, max, default, taper, unit, variants?}`. JSON
/// generation is one place so a future schema bump (e.g. adding a `module`
/// field) stays self-contained.
fn build_faceplate_html() -> String {
    PLACEHOLDER_HTML
        .replace("__PARAMS_JSON__", &build_params_json())
        .replace("__SUBDIVISIONS_JSON__", &build_subdivisions_json())
        .replace("__PATCH_COUNT__", &PATCH_COUNT.to_string())
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
        ViewEvent::EditLayerChanged { layer } => json!({
            "kind": "edit_layer_changed",
            "layer": match layer { Layer::Upper => "upper", Layer::Lower => "lower" },
        }),
        ViewEvent::Status { line } => json!({
            "kind": "status",
            "line": line,
        }),
    };
    v.to_string()
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

/// Static HTML/CSS faceplate scaffold (0040). Four-row panel grid with empty
/// bodies; 0041+ populates each panel with controls. Inline `<style>` block
/// keeps the page openable in a browser for visual previewing without the
/// wry runtime.
const PLACEHOLDER_HTML: &str = include_str!("../assets/faceplate.html");

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

    fn count(needle: &str) -> usize {
        PLACEHOLDER_HTML.matches(needle).count()
    }

    #[test]
    fn faceplate_has_banner_and_preset_bar_slot() {
        assert!(PLACEHOLDER_HTML.contains(r#"class="banner""#));
        assert!(PLACEHOLDER_HTML.contains("VULPUS LABS"));
        assert!(PLACEHOLDER_HTML.contains("VXN-1"));
        assert!(PLACEHOLDER_HTML.contains(r#"class="preset-bar-slot""#));
    }

    #[test]
    fn faceplate_has_four_rows() {
        for r in 1..=4 {
            assert!(
                PLACEHOLDER_HTML.contains(&format!(r#"data-row="{r}""#)),
                "missing data-row=\"{r}\"",
            );
        }
        // Five panels per row × 4 rows = 20 panels total. Catches an
        // accidental row collapse or duplicate emit.
        assert_eq!(count(r#"class="panel""#), 20, "panel count drift");
    }

    #[test]
    fn faceplate_panel_names_match_rows() {
        // Same titles as `vxn_ui_vizia::ROWS`; reordering or rename would have to
        // happen here in lockstep.
        let expected: &[&[&str]] = &[
            &["LFO 1", "LFO 2", "Osc 1", "Osc 2", "Mixer"],
            &["Env 1", "Env 2", "VCA", "Filter", "Filter Mod"],
            &["Pitch Mod", "PWM Mod", "Cross Mod", "Mod Wheel", "Bend"],
            &["Keys", "Voice", "Chorus", "Delay", "Master"],
        ];
        for row in expected {
            for name in *row {
                assert!(
                    PLACEHOLDER_HTML.contains(&format!(r#"data-name="{name}""#)),
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
                PLACEHOLDER_HTML.contains(&marker),
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
    fn faceplate_reserves_chorus_delay_header_toggle() {
        // Header switch lives on Chorus + Delay only (`vxn_ui_vizia::panel_view`,
        // `header_switch` matcher). Reserve the slot now; widget arrives in
        // 0045.
        for name in ["Chorus", "Delay"] {
            assert!(
                PLACEHOLDER_HTML
                    .contains(&format!(r#"data-name="{name}" data-header-toggle"#)),
                "{name} missing data-header-toggle",
            );
        }
        // `data-header-toggle>` matches the panel-div attribute only;
        // CSS `[data-header-toggle]` selectors don't have the closing `>`.
        assert_eq!(
            count("data-header-toggle>"),
            2,
            "header-toggle expected on Chorus + Delay only",
        );
    }

    #[test]
    fn faceplate_css_vars_match_vxn_ui_vizia_constants() {
        // Pixel literals live in CSS vars (ticket: "a future resize policy
        // should be one variable change"). Sanity check the load-bearing
        // ones against `vxn_ui_vizia` constants.
        assert!(PLACEHOLDER_HTML.contains("--panel-h: 156px"));
        assert!(PLACEHOLDER_HTML.contains("--col-h: 120px"));
        assert!(PLACEHOLDER_HTML.contains("--fader-h: 74px"));
        assert!(PLACEHOLDER_HTML.contains("--dial: 62px"));
        assert!(PLACEHOLDER_HTML.contains("--banner-h: 26px"));
        assert!(PLACEHOLDER_HTML.contains("--preset-bar-h: 30px"));
        assert!(PLACEHOLDER_HTML.contains("--pad-outer: 10px"));
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
                PLACEHOLDER_HTML
                    .contains(&format!(r#".panel[data-name="{sel}"]"#))
                    && PLACEHOLDER_HTML.contains(&format!("flex-grow: {share}")),
                "share for {sel} ≠ {share}",
            );
        }
        // Bend is the only fixed-width panel.
        assert!(PLACEHOLDER_HTML.contains("flex: 0 0 54px"));
    }

    #[test]
    fn faceplate_bridge_object_intact() {
        // Bridge from 0039 still present — 0040 only adds layout.
        assert!(PLACEHOLDER_HTML.contains("window.vxn"));
        assert!(PLACEHOLDER_HTML.contains("window.ipc.postMessage"));
        assert!(PLACEHOLDER_HTML.contains("onViewEvent"));
    }

    #[test]
    fn faceplate_batched_bridge_wired() {
        // 0046: Rust calls `window.__vxn.applyViewEvents(arr)` once per
        // controller tick. Bootstrap installs a buffering stub; init() swaps
        // in the real dispatcher.
        assert!(PLACEHOLDER_HTML.contains("window.__vxn"));
        assert!(PLACEHOLDER_HTML.contains("applyViewEvents"));
        // Bootstrap stub still funnels into `_earlyViewEvents` so events
        // that race the inline init() are not lost.
        assert!(PLACEHOLDER_HTML.contains("_earlyViewEvents"));
    }

    #[test]
    fn faceplate_status_pill_wired() {
        // 0046: Status ViewEvent flashes a pill in the lower-right corner.
        assert!(PLACEHOLDER_HTML.contains(".status-pill"));
        assert!(PLACEHOLDER_HTML.contains(".status-pill.visible"));
        assert!(PLACEHOLDER_HTML.contains("statusPill"));
        assert!(PLACEHOLDER_HTML.contains("statusPill.flash"));
        assert!(PLACEHOLDER_HTML.contains("ev.kind === 'status'"));
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
            ("fader",  "ring_level",  "Ring"),
            ("fader",  "noise_level", "Noise"),
            ("switch", "noise_color", "Col"),
        ] {
            let marker = format!(
                r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
            );
            assert!(
                PLACEHOLDER_HTML.contains(&marker),
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
                PLACEHOLDER_HTML.contains(&marker),
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
            ("fader",       "pitch_lfo_depth", "Depth"),
            ("buttongroup", "pitch_lfo_src",   "LFO"),
            ("fader",       "pitch_env_depth", "Depth"),
            ("buttongroup", "pitch_env_src",   "Env"),
            // PWM Mod
            ("fader",       "pwm_lfo_depth", "Depth"),
            ("buttongroup", "pwm_lfo_src",   "LFO"),
            ("fader",       "pwm_env_depth", "Depth"),
            ("buttongroup", "pwm_env_src",   "Env"),
            // Cross Mod
            ("buttongroup", "cross_mod_type",       "Type"),
            ("fader",       "cross_mod_amount",     "Amt"),
            ("buttongroup", "osc2_pitch_env_src",   "Src"),
            ("fader",       "osc2_pitch_env_depth", "Mod"),
            // Mod Wheel
            ("fader", "mod_wheel_pwm",        "PWM"),
            ("fader", "mod_wheel_cutoff",     "Cutoff"),
            ("fader", "mod_wheel_reso",       "Reso"),
            ("fader", "mod_wheel_osc2_pitch", "O2 Pitch"),
            // Bend
            ("fader", "pitch_wheel_depth", "Range"),
        ] {
            let marker = format!(
                r#"data-control="{kind}" data-param="{name}" data-label="{label}""#,
            );
            assert!(
                PLACEHOLDER_HTML.contains(&marker),
                "Row 3 mount point missing: {marker}",
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
                PLACEHOLDER_HTML.contains(&needle),
                "Row 4 mount point missing: {needle}",
            );
        }
        // Voice's AssignMode buttongroup carries the display permutation
        // (descriptor order = Poly/Unison/Solo/Twin → display order =
        // Poly/Twin/Unison/Solo). If the descriptor order changes, this
        // attribute changes alongside; the test guards the wiring.
        assert!(
            PLACEHOLDER_HTML.contains(r#"data-param="assign_mode" data-label="Assign" data-order="0,3,1,2""#),
            "AssignMode missing display-order remap",
        );
        // Detune-Legato carries its two extra param-name dependencies so
        // a layer rebind can re-resolve both alongside the primary param.
        assert!(
            PLACEHOLDER_HTML.contains(r#"data-legato-param="legato""#),
            "Detune-Legato missing data-legato-param",
        );
        assert!(
            PLACEHOLDER_HTML.contains(r#"data-mode-param="assign_mode""#),
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
        //   Row 1: LFO1 3 (Rate/Delay/Fade), LFO2 1 (Rate), Osc1 4, Osc2 4, Mixer 4 = 16
        //   Row 2: Env1 4, Env2 4, VCA 1, Filter 4, FilterMod 4              = 17
        //   Row 3: PitchMod 2, PwmMod 2, CrossMod 2, ModWheel 4, Bend 1      = 11
        //   Row 4: Voice 1 (Glide), Master 2, Chorus 3, Delay 3              =  9
        //   Total = 53.
        // Waves: 4 (LFO 1/2 Shape, Osc 1/2 Wave).
        // Switches:
        //   Row 1: 4 (LfoSync, Lfo2Sync, Lfo1FreeRun, NoiseColor)
        //   Row 2: 5 (Env1Shape, Env2Shape, Gate, Slope, KeyTrk)
        //   Row 4: 4 (Oversample as multi-toggle row, LimiterOn,
        //            DelaySync, DelayPingPong)
        //   Total = 13.
        // Button groups:
        //   Row 2: 2 (AmpLfoSrc, FilterMode)
        //   Row 3: 6 (Pitch/PWM LFO+Env sources, CrossModType, Osc2PitchEnvSrc)
        //   Row 4: 1 (AssignMode) — Oversample renders as a horizontal
        //     switch row at the bottom of Master, not a vertical
        //     buttongroup column.
        //   Total = 9.
        // Header switches: 2 (Chorus, Delay).
        // Detune-Legato composite: 1 (Voice).
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="fader""#).count(),
            53,
            "expected 53 fader cells across all four rows",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="wave""#).count(),
            4,
            "expected 4 wave cells (LFO 1, LFO 2, Osc 1, Osc 2)",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="switch""#).count(),
            13,
            "expected 13 switch cells (Row 1 + Row 2 + Row 4)",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="buttongroup""#).count(),
            9,
            "expected 9 buttongroup cells (Row 2 + Row 3 + Row 4)",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="dropdown""#).count(),
            0,
            "no dropdown cells expected (all enums fit ButtonGroup)",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="header-switch""#).count(),
            2,
            "expected 2 header-switch cells (Chorus, Delay)",
        );
        assert_eq!(
            PLACEHOLDER_HTML.matches(r#"data-control="detune-legato""#).count(),
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
            PLACEHOLDER_HTML.contains(r#"data-dim-unless-fm="cross_mod_type""#),
            "Cross Mod Amount missing data-dim-unless-fm wiring",
        );
        for (depth, src) in [
            ("osc2_pitch_env_depth", "osc2_pitch_env_src"),
            ("pitch_lfo_depth",      "pitch_lfo_src"),
            ("pitch_env_depth",      "pitch_env_src"),
            ("pwm_lfo_depth",        "pwm_lfo_src"),
            ("pwm_env_depth",        "pwm_env_src"),
            // VCA's Amp LFO Depth follows the same rule (Off / LFO 1 /
            // LFO 2 source → fader dims on Off).
            ("amp_lfo_depth",        "amp_lfo_src"),
        ] {
            assert!(
                PLACEHOLDER_HTML.contains(&format!(
                    r#"data-param="{depth}" data-label="{}" data-dim-when-src-off="{src}""#,
                    if depth == "osc2_pitch_env_depth" { "Mod" } else { "Depth" },
                )),
                "route depth {depth} missing dim-when-src-off=\"{src}\"",
            );
        }
        // Route-column source selectors must NOT carry the self-dim
        // marker — selectors stay bright; only the paired fader dims.
        assert_eq!(
            PLACEHOLDER_HTML.matches("data-dim-when-zero").count(),
            0,
            "route-col source selectors should no longer self-dim",
        );
        // JS dispatch wires the generic dim rule into ParamChanged.
        assert!(PLACEHOLDER_HTML.contains("applyDimRulesFor("));
        assert!(PLACEHOLDER_HTML.contains("collectDimRuleSpecs"));
    }

    #[test]
    fn edit_layer_rebind_wired() {
        // 0045: EditLayerChanged ViewEvent dispatch + layer-rebind logic
        // present. The actual rebind walks LAYERED_CELLS and re-resolves
        // each per-patch name → id via paramIdByNameAtLayer using the
        // patchCount splice.
        assert!(PLACEHOLDER_HTML.contains("edit_layer_changed"));
        assert!(PLACEHOLDER_HTML.contains("rebindAllForLayer"));
        assert!(PLACEHOLDER_HTML.contains("paramIdByNameAtLayer"));
        assert!(PLACEHOLDER_HTML.contains("__PATCH_COUNT__"));
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
        assert!(PLACEHOLDER_HTML.contains("makeHeaderSwitch"));
        assert!(PLACEHOLDER_HTML.contains(".panel-header-switch"));
        assert!(PLACEHOLDER_HTML.contains(".panel-header-switch.active"));
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
        // Asserting on PLACEHOLDER_HTML keeps the test substring-based —
        // the existing Free-run dim has the same shape.
        assert!(
            PLACEHOLDER_HTML.contains(".ctl-strip.dimmed"),
            "missing strip dim selector (slope dim relies on it)",
        );
        assert!(PLACEHOLDER_HTML.contains("locateSlopeDimCells"));
        assert!(PLACEHOLDER_HTML.contains("FILTER_MODE_ID = paramIdByNameAtLayer('filter_mode'"));
        assert!(PLACEHOLDER_HTML.contains("variants.indexOf('Notch')"));
        assert!(PLACEHOLDER_HTML.contains("data-param=\"filter_slope\""));
        assert!(PLACEHOLDER_HTML.contains("ev.id === FILTER_MODE_ID"));
    }

    #[test]
    fn faceplate_has_subdivisions_json_placeholder() {
        // SUBDIVISIONS table is spliced as a JSON array of labels; the LFO
        // rate fader's displayOverride indexes it when sync is on (0042).
        assert!(PLACEHOLDER_HTML.contains("__SUBDIVISIONS_JSON__"));
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
        assert!(PLACEHOLDER_HTML.contains("__PARAMS_JSON__"));
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
}
