//! VXN1 editor (Vizia), embedded into the host window via baseview.
//!
//! Laid out as a Jupiter-8-style faceplate: bordered, header-labelled panels
//! arranged in rows, mostly small vertical faders, with compact labels. Each
//! parameter picks a widget: a vertical [`Slider`] for continuous (float/int)
//! params; a rotary [`Knob`] selector for waveform/colour/shape enums; a
//! [`ButtonGroup`] for the oversample enum; a (vertical) [`Switch`] for bools
//! and two-option enums; a [`Select`] dropdown for any remaining enum. Value
//! readouts use the shared [`vxn_engine::ParamDesc::display`] so the editor and
//! the host's generic UI read identically.
//!
//! Parameter flow (see `vxn_engine::SharedParams` and `vxn-clap`'s
//! `LocalParams`):
//!
//! - **UI → host:** a control's callback writes the new value into the shared
//!   store; faders raise a gesture on pointer down/up. The plugin's audio thread
//!   diffs the store each `process` and emits the change (bracketed by the
//!   gesture) to the host, so DAW automation recording stays in sync.
//! - **host → UI:** [`Application::on_idle`] emits a poll; [`UiModel`] reads the
//!   shared store back into the reactive [`SyncSignal`]s so controls track live
//!   automation. Signals are created on the UI thread (scoped to the view tree)
//!   and reached from `on_idle` via the model, avoiding leaks.
//!
//! Fader signals hold the *normalized* `[0, 1]` value; the shared store converts
//! to/from plain units via the parameter descriptors.
//!
//! The 6×4 modulation matrix is surfaced economically: only the musically
//! useful routes get dedicated faders, placed in context (filter mods in the
//! Filter panel, vibrato/pitch-env/PWM in VCO Mod, velocity/tremolo in Amp).
//! Both LFO rows ride this layout — LFO 1's routes plus the LFO 2 row's four
//! per-patch depths (`Lfo2*`) sit beside their LFO 1 counterparts. The remaining
//! cells stay engine-only but host-automatable.
//!
//! The two LFOs are asymmetric (E005): LFO 1 is per-voice with a delay→fade
//! onset and a free-run toggle (its own panel), while LFO 2 is one global
//! instrument-wide oscillator (a global panel). Both expose a host-sync toggle;
//! with sync on, the rate readout shows the musical subdivision instead of Hz.

use std::ffi::c_void;
use std::sync::Arc;

use vizia::ParentWindow;
use vizia::context::TreeProps;
use vizia::prelude::*;
use vizia::vg;
use vxn_engine::{
    GlobalParam, KeyMode, Layer, ParamKind, ParamRef, PatchParam, SharedParams, TOTAL_PARAMS,
    desc_for_clap_id, global_clap_id, param_ref, patch_clap_id,
};

/// Resolve a faceplate [`Entry`]'s baked (Upper) CLAP id to the layer currently
/// being edited: per-patch entries re-point to `layer`'s block, global entries
/// stay fixed. This is the binding indirection behind the Upper/Lower toggle
/// (ADR 0003 §6) — a UI view switch, never a parameter change.
fn resolve(entry_id: usize, layer: Layer) -> usize {
    match param_ref(entry_id) {
        Some(ParamRef::Patch(_, p)) => patch_clap_id(layer, p),
        _ => entry_id,
    }
}

/// Whether a panel's entries bind to a layer's per-patch block (so the panel
/// follows the Upper/Lower toggle) rather than the fixed global block.
fn is_layer_dependent(entries: &[Entry]) -> bool {
    entries
        .iter()
        .any(|(id, _)| matches!(param_ref(*id), Some(ParamRef::Patch(..))))
}

/// MIDI note number → name (e.g. 60 → "C4"), for the split-point readout.
fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = n as i32 / 12 - 1;
    format!("{}{}", NAMES[(n % 12) as usize], octave)
}

/// Handle to the live editor window. Call [`WindowHandle::close`] when the host
/// destroys the GUI.
pub type EditorHandle = WindowHandle;

pub const EDITOR_WIDTH: u32 = 820;
/// Four panel rows now (LFO 1 / LFO 2 split out the effects onto their own row).
pub const EDITOR_HEIGHT: u32 = 600;

/// A control entry: CLAP id plus a short faceplate label (the panel header
/// supplies the context, so per-control labels stay terse). Entries are baked
/// against the **Upper** layer; [`resolve`] re-points per-patch entries to the
/// layer chosen by the Upper/Lower edit toggle (global entries stay fixed).
type Entry = (usize, &'static str);

/// Faceplate layout: rows of panels, each panel a titled group of controls.
/// Mod-matrix routes appear as dedicated faders in context (VCO Mod / Filter /
/// Amp panels), not as a generic grid.
/// Upper-layer per-patch CLAP id; [`resolve`] swaps it to Lower when that layer
/// is the edit target.
const fn u(p: PatchParam) -> usize {
    patch_clap_id(Layer::Upper, p)
}
/// Global-param CLAP id.
const fn g(p: GlobalParam) -> usize {
    global_clap_id(p)
}

const ROWS: &[&[(&str, &[Entry])]] = {
    use GlobalParam::{
        ChorusDepth, ChorusMix, ChorusOn, ChorusRate, DelayFeedback, DelayMix, DelayOn,
        DelayPingPong, DelayTime, Lfo2Rate, Lfo2Shape, Lfo2Sync, MasterTune, MasterVolume,
        Oversample,
    };
    use PatchParam::*;
    &[
        &[
            (
                "Osc 1",
                &[
                    (u(Osc1Wave), "Wave"),
                    (u(Osc1Coarse), "Coarse"),
                    (u(Osc1Fine), "Fine"),
                    (u(Osc1Level), "Level"),
                    (u(Osc1PulseWidth), "PW"),
                ],
            ),
            (
                "Osc 2",
                &[
                    (u(Osc2Wave), "Wave"),
                    (u(Osc2Coarse), "Coarse"),
                    (u(Osc2Fine), "Fine"),
                    (u(Osc2Level), "Level"),
                    (u(Osc2PulseWidth), "PW"),
                ],
            ),
            (
                "Noise",
                &[(u(NoiseColor), "Color"), (u(NoiseLevel), "Level")],
            ),
            (
                "VCO Mod",
                &[
                    (u(LfoPitch), "Vib"),
                    (u(Lfo2Pitch), "Vib2"),
                    (u(Env1Pitch), "P.Env"),
                    (u(LfoPwm), "PWM"),
                    (u(Lfo2Pwm), "PWM2"),
                ],
            ),
        ],
        &[
            (
                "Filter",
                &[
                    (u(Cutoff), "Cutoff"),
                    (u(Resonance), "Reso"),
                    (u(Drive), "Drive"),
                    (u(Env1Cutoff), "Env"),
                    (u(KeyCutoff), "Key"),
                    (u(LfoCutoff), "LFO"),
                    (u(Lfo2Cutoff), "LFO2"),
                    (u(VelCutoff), "Vel"),
                    (u(FilterVariant), "Type"),
                ],
            ),
            (
                "Env 1",
                &[
                    (u(Env1Attack), "A"),
                    (u(Env1Decay), "D"),
                    (u(Env1Sustain), "S"),
                    (u(Env1Release), "R"),
                    (u(Env1Shape), "Shape"),
                ],
            ),
            (
                "Env 2",
                &[
                    (u(Env2Attack), "A"),
                    (u(Env2Decay), "D"),
                    (u(Env2Sustain), "S"),
                    (u(Env2Release), "R"),
                    (u(Env2Shape), "Shape"),
                ],
            ),
        ],
        &[
            (
                // LFO 1 — per-voice (E005 / 0018): shape/rate/sync plus the
                // per-voice delay→fade onset and free-run toggle.
                "LFO 1",
                &[
                    (u(LfoShape), "Shape"),
                    (u(LfoRate), "Rate"),
                    (u(LfoSync), "Sync"),
                    (u(Lfo1DelayTime), "Delay"),
                    (u(Lfo1Fade), "Fade"),
                    (u(Lfo1FreeRun), "Free"),
                ],
            ),
            (
                // LFO 2 — one global instrument-wide oscillator (E005 / 0019);
                // shape/rate/sync are global (its routing depths live in the
                // VCO Mod / Filter / Amp panels as the LFO 2 matrix row).
                "LFO 2",
                &[
                    (g(Lfo2Shape), "Shape"),
                    (g(Lfo2Rate), "Rate"),
                    (g(Lfo2Sync), "Sync"),
                ],
            ),
            (
                "Amp",
                &[
                    (u(VelAmp), "Vel"),
                    (u(LfoAmp), "Trem"),
                    (u(Lfo2Amp), "Trem2"),
                ],
            ),
        ],
        &[
            (
                "Master",
                &[
                    (g(MasterTune), "Tune"),
                    (g(MasterVolume), "Volume"),
                    (g(Oversample), "OvSmp"),
                ],
            ),
            (
                "Chorus",
                &[
                    (g(ChorusOn), "On"),
                    (g(ChorusRate), "Rate"),
                    (g(ChorusDepth), "Depth"),
                    (g(ChorusMix), "Mix"),
                ],
            ),
            (
                "Delay",
                &[
                    (g(DelayOn), "On"),
                    (g(DelayTime), "Time"),
                    (g(DelayFeedback), "FB"),
                    (g(DelayMix), "Mix"),
                    (g(DelayPingPong), "Ping"),
                ],
            ),
        ],
    ]
};

/// Stylesheet: dark faceplate, orange panel headers, small text.
const STYLE: &str = r#"
:root { background-color: #2b2b2b; font-family: "IBM Plex Sans Condensed Medium"; }
label { font-size: 11; color: #d6d6d6; }
.panel { background-color: #1c1c1c; border-width: 1px; border-color: #0e0e0e; corner-radius: 4px; }
.panel-header { background-color: #d9701b; color: #141414; corner-radius: 2px; }
.ctl-label { font-size: 9; color: #aeaeae; }
.ctl-value { font-size: 9; color: #d9701b; }
.vswitch { rotate: 270deg; top: 20px; }
.ovsmp { gap: 2px; }
.ovsmp toggle-button { background-color: #555555; padding: 3px; }
.ovsmp toggle-button:checked { background-color: #2e9e3f; }
.ovsmp toggle-button label { color: #ffffff; font-size: 9; }
.value-pop { background-color: #0e0e0e; border-width: 1px; border-color: #d9701b; corner-radius: 3px; padding-left: 4px; padding-right: 4px; font-size: 10; color: #f6f6f6; }
.fader .track { background-color: #555555; width: 6px; corner-radius: 2px; }
.fader .range { background-color: #d9701b; corner-radius: 2px; }
.fader .thumb { background-color: #e8e8e8; border-width: 1px; border-color: #141414; corner-radius: 1px; width: 20px; height: 8px; }
.wave-glyph { color: #888888; }
.wave-glyph.active { color: #e8902f; }
.wave-txt { font-size: 8; color: #888888; }
.wave-txt.active { color: #e8902f; }
"#;

const FADER_H: f32 = 66.0;
const COL_H: f32 = 98.0;
const PANEL_H: f32 = 124.0;
/// Square area framing a selector knob, sized to fit the variant glyphs/labels
/// arranged around its arc.
const DIAL: f32 = 62.0;

/// UI value range for a fader. Bipolar routes (env→cutoff, env→pitch) use the
/// full descriptor range, centred at zero; the unipolar mod amounts
/// (key/LFO/velocity→cutoff, LFO→PWM, velocity/LFO→amp — both LFO rows) are
/// shown positive-only (`0..max`) even though the underlying depth param is
/// bipolar. LFO→pitch is handled separately (see [`is_lfo_pitch`]).
fn ui_range(idx: usize) -> (f32, f32) {
    use PatchParam::*;
    let Some(d) = desc_for_clap_id(idx) else {
        return (0.0, 1.0);
    };
    match param_ref(idx) {
        Some(ParamRef::Patch(
            _,
            KeyCutoff | LfoCutoff | VelCutoff | LfoPwm | VelAmp | LfoAmp | Lfo2Cutoff | Lfo2Amp
            | Lfo2Pwm,
        )) => (0.0, d.max),
        _ => (d.min, d.max),
    }
}

/// LFO→pitch routes (`LfoPitch` / `Lfo2Pitch`) get a narrowed, curved fader for
/// musical vibrato rather than the route's full ±48 st: it tops out at a whole
/// semitone and bends so the half-way point sits at ~0.2 st, keeping the gentle
/// useful range spread across most of the travel. The underlying depth param
/// still spans ±48 st for automation/presets — this only shapes the editor fader.
fn is_lfo_pitch(idx: usize) -> bool {
    matches!(
        param_ref(idx),
        Some(ParamRef::Patch(
            _,
            PatchParam::LfoPitch | PatchParam::Lfo2Pitch
        ))
    )
}

/// Whole-semitone ceiling for the LFO→pitch faders.
const LFO_PITCH_MAX: f32 = 1.0;
/// Curve exponent placing the fader midpoint at ~0.2 st: `ln(0.2)/ln(0.5)`.
const LFO_PITCH_CURVE: f32 = 2.321_928;

/// Envelope time faders (attack / decay / release on both envelopes) get an
/// exponential taper rather than the descriptor's linear 0.001..10 s, so the
/// busy short-time region isn't crammed into the bottom of the travel. The
/// curve `t = A·(e^(K·n) − 1)` is pinned through two anchors: the fader midpoint
/// reads **1 s** and the top reads **10 s** (the JP-8's full range) — i.e. the
/// lower half spans 0–1 s, the upper half 1–10 s. Sustain (a level, not a time)
/// keeps the plain linear map.
fn is_adsr_time(idx: usize) -> bool {
    use PatchParam::*;
    matches!(
        param_ref(idx),
        Some(ParamRef::Patch(
            _,
            Env1Attack | Env1Decay | Env1Release | Env2Attack | Env2Decay | Env2Release
        ))
    )
}

/// `K = 2·ln(9)`; with `A` below this puts the midpoint at 1 s and the top at 10 s.
const ADSR_K: f32 = 4.394_449;
/// `A = 1/(e^(K/2) − 1) = 1/8`.
const ADSR_A: f32 = 0.125;

/// Plain value → fader position `[0, 1]` over the UI range.
fn fader_to_ui(idx: usize, value: f32) -> f32 {
    if is_lfo_pitch(idx) {
        return (value / LFO_PITCH_MAX)
            .clamp(0.0, 1.0)
            .powf(1.0 / LFO_PITCH_CURVE);
    }
    if is_adsr_time(idx) {
        // Inverse of `A·(e^(K·n) − 1)`.
        return ((value / ADSR_A + 1.0).ln() / ADSR_K).clamp(0.0, 1.0);
    }
    let (lo, hi) = ui_range(idx);
    if hi > lo {
        ((value - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Fader position `[0, 1]` → plain value over the UI range.
fn fader_from_ui(idx: usize, n: f32) -> f32 {
    if is_lfo_pitch(idx) {
        return n.clamp(0.0, 1.0).powf(LFO_PITCH_CURVE) * LFO_PITCH_MAX;
    }
    if is_adsr_time(idx) {
        return ADSR_A * ((ADSR_K * n.clamp(0.0, 1.0)).exp() - 1.0);
    }
    let (lo, hi) = ui_range(idx);
    lo + n.clamp(0.0, 1.0) * (hi - lo)
}

/// The host-sync toggle paired with an LFO rate fader, if `idx` is one. With
/// that toggle on, the rate knob's position selects a musical subdivision
/// (E004 / 0015), so the rate readout shows the subdivision label instead of Hz.
/// LFO 1's rate/sync are per-patch (same layer); LFO 2's are global.
fn sync_partner(idx: usize) -> Option<usize> {
    match param_ref(idx) {
        Some(ParamRef::Patch(layer, PatchParam::LfoRate)) => {
            Some(patch_clap_id(layer, PatchParam::LfoSync))
        }
        Some(ParamRef::Global(GlobalParam::Lfo2Rate)) => {
            Some(global_clap_id(GlobalParam::Lfo2Sync))
        }
        _ => None,
    }
}

/// A bound control and its reactive value signal, kept so `on_idle` can sync
/// the signal from host-side automation.
#[derive(Clone, Copy)]
enum Ctl {
    /// Continuous (float/int) → vertical fader; signal holds the normalized value.
    Fader(usize, SyncSignal<f32>),
    /// Osc waveform → rotary selector; signal holds the normalized value, snapped
    /// to the nearest variant on change.
    Rotary(usize, SyncSignal<f32>),
    /// Bool or two-variant enum → vertical switch; signal holds the on/off state.
    Switch(usize, SyncSignal<bool>),
    /// Enum → exclusive button group; signal holds the selected variant index.
    Buttons(usize, SyncSignal<Option<usize>>),
    /// Enum → dropdown; signal holds the selected variant index.
    Select(usize, SyncSignal<Option<usize>>),
}

impl Ctl {
    fn idx(self) -> usize {
        match self {
            Ctl::Fader(i, _)
            | Ctl::Rotary(i, _)
            | Ctl::Switch(i, _)
            | Ctl::Buttons(i, _)
            | Ctl::Select(i, _) => i,
        }
    }
}

fn make_ctl(i: usize, shared: &SharedParams) -> Ctl {
    let Some(desc) = desc_for_clap_id(i) else {
        return Ctl::Fader(i, SyncSignal::new(0.0));
    };
    // Rotary for the waveform / LFO-shape selectors; buttons for Oversample —
    // detected on the typed param so it holds across both layers (and the global
    // LFO 2 shape).
    let is_rotary = matches!(
        param_ref(i),
        Some(ParamRef::Patch(
            _,
            PatchParam::Osc1Wave | PatchParam::Osc2Wave | PatchParam::LfoShape
        )) | Some(ParamRef::Global(GlobalParam::Lfo2Shape))
    );
    let is_buttons = matches!(
        param_ref(i),
        Some(ParamRef::Global(GlobalParam::Oversample))
    );
    match desc.kind {
        ParamKind::Bool => Ctl::Switch(i, SyncSignal::new(shared.get(i) >= 0.5)),
        // Waveform / colour / shape selectors are rotary; Oversample is a button
        // group; two-option enums are switches; anything else a dropdown.
        ParamKind::Enum { variants } => {
            if is_rotary {
                Ctl::Rotary(i, SyncSignal::new(shared.get_normalized(i)))
            } else if is_buttons {
                Ctl::Buttons(i, SyncSignal::new(Some(shared.get(i).round() as usize)))
            } else if variants.len() == 2 {
                Ctl::Switch(i, SyncSignal::new(shared.get(i) >= 0.5))
            } else {
                Ctl::Select(i, SyncSignal::new(Some(shared.get(i).round() as usize)))
            }
        }
        _ => Ctl::Fader(i, SyncSignal::new(fader_to_ui(i, shared.get(i)))),
    }
}

/// Poll message emitted from `on_idle`: re-read the shared store into signals.
struct PollAutomation;

/// Bridges `on_idle` polling to the control signals so DAW automation playback
/// moves the controls. Edits flow the other way directly via each callback.
struct UiModel {
    controls: Vec<Ctl>,
    shared: Arc<SharedParams>,
    /// Mirrors of the non-automatable key-mode state, re-synced from the store so
    /// the Keys panel tracks a state load (the UI is the only other writer).
    key_mode: SyncSignal<usize>,
    split: SyncSignal<f32>,
}

impl Model for UiModel {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|_msg: &PollAutomation, _meta| {
            let km = self.shared.key_mode() as usize;
            if self.key_mode.get() != km {
                self.key_mode.set(km);
            }
            let sp = self.shared.split_point() as f32;
            if (self.split.get() - sp).abs() > f32::EPSILON {
                self.split.set(sp);
            }
            for ctl in &self.controls {
                match *ctl {
                    Ctl::Fader(i, sig) => {
                        let n = fader_to_ui(i, self.shared.get(i));
                        if (sig.get() - n).abs() > f32::EPSILON {
                            sig.set(n);
                        }
                    }
                    Ctl::Rotary(i, sig) => {
                        let n = self.shared.get_normalized(i);
                        if (sig.get() - n).abs() > f32::EPSILON {
                            sig.set(n);
                        }
                    }
                    Ctl::Switch(i, sig) => {
                        let b = self.shared.get(i) >= 0.5;
                        if sig.get() != b {
                            sig.set(b);
                        }
                    }
                    Ctl::Buttons(i, sig) | Ctl::Select(i, sig) => {
                        let s = Some(self.shared.get(i).round() as usize);
                        if sig.get() != s {
                            sig.set(s);
                        }
                    }
                }
            }
        });
    }
}

/// Open the editor parented to `parent` (on macOS the host `NSView`).
pub fn open_editor(parent: *mut c_void, shared: Arc<SharedParams>) -> EditorHandle {
    let parent = ParentWindow(parent);
    Application::new(move |cx| build_editor(cx, Arc::clone(&shared)))
        .on_idle(|cx| cx.emit(PollAutomation))
        .inner_size((EDITOR_WIDTH, EDITOR_HEIGHT))
        .title("VXN1")
        .open_parented(&parent)
}

fn build_editor(cx: &mut Context, shared: Arc<SharedParams>) {
    // Bundle the faceplate font so it renders identically on any host/OS. Each
    // weight is its own family ("IBM Plex Sans Condensed {Thin|ExtraLight|
    // Medium}"); referenced by name in STYLE.
    cx.add_font_mem(include_bytes!("../fonts/IBMPlexSansCondensed-Thin.ttf"));
    cx.add_font_mem(include_bytes!(
        "../fonts/IBMPlexSansCondensed-ExtraLight.ttf"
    ));
    cx.add_font_mem(include_bytes!("../fonts/IBMPlexSansCondensed-Medium.ttf"));
    let _ = cx.add_stylesheet(STYLE);

    // One control per CLAP id, across both layers + global (panels look them up
    // by resolved id; mod-matrix cells and per-layer params not on the faceplate
    // stay engine-only but host-automatable). The model syncs them on idle.
    let controls: Vec<Ctl> = (0..TOTAL_PARAMS).map(|i| make_ctl(i, &shared)).collect();

    // Key-mode UI state (ADR 0003 §6). `edit_layer` is pure view state; `key_mode`
    // and `split` mirror the non-automatable shared state (set via the state path,
    // not param gestures) and are re-synced from the store on idle.
    let edit_layer = SyncSignal::new(0usize);
    let key_mode = SyncSignal::new(shared.key_mode() as usize);
    let split = SyncSignal::new(shared.split_point() as f32);

    UiModel {
        controls: controls.clone(),
        shared: Arc::clone(&shared),
        key_mode,
        split,
    }
    .build(cx);

    let last_row = ROWS.len() - 1;
    ScrollView::new(cx, move |cx| {
        VStack::new(cx, |cx| {
            for (r, row) in ROWS.iter().enumerate() {
                HStack::new(cx, |cx| {
                    for (title, entries) in *row {
                        if is_layer_dependent(entries) {
                            // Build the panel for each layer; show only the one
                            // matching the edit-target toggle (no structural rebuild).
                            for layer in Layer::ALL {
                                let li = layer as usize;
                                let vis = edit_layer.map(move |l: &usize| *l == li);
                                panel_view(
                                    cx,
                                    title,
                                    entries,
                                    layer,
                                    &controls,
                                    &shared,
                                    Some(vis),
                                );
                            }
                        } else {
                            panel_view(cx, title, entries, Layer::Upper, &controls, &shared, None);
                        }
                    }
                    // The key-mode panel rides in the last row.
                    if r == last_row {
                        keys_panel(cx, &shared, edit_layer, key_mode, split);
                    }
                })
                .height(Pixels(PANEL_H))
                .horizontal_gap(Pixels(8.0));
            }
        })
        .vertical_gap(Pixels(8.0))
        .padding(Pixels(10.0));
    });
}

/// The "Keys" panel: key-mode selector, Upper/Lower edit-target toggle (hidden
/// in Whole), and split-point control (shown in Split). The mode and split write
/// the **non-automatable** shared state directly (ADR 0003 §3/§8) — not param
/// gestures — so they neither echo to the host as automation nor record a knob
/// move; the edit toggle is pure view state.
fn keys_panel(
    cx: &mut Context,
    shared: &Arc<SharedParams>,
    edit_layer: SyncSignal<usize>,
    key_mode: SyncSignal<usize>,
    split: SyncSignal<f32>,
) {
    const MODES: [&str; 3] = ["Whole", "Dual", "Split"];
    const EDIT: [&str; 2] = ["Upper", "Lower"];
    VStack::new(cx, |cx| {
        Label::new(cx, "Keys")
            .class("panel-header")
            .width(Stretch(1.0))
            .height(Pixels(16.0))
            .alignment(Alignment::Center);
        VStack::new(cx, move |cx| {
            // Key-mode selector. Choosing Whole snaps the edit target back to
            // Upper (the toggle is hidden), so we never edit a hidden Lower.
            let sh_mode = Arc::clone(shared);
            ButtonGroup::new(cx, move |cx| {
                for (n, label) in MODES.iter().enumerate() {
                    let sh = Arc::clone(&sh_mode);
                    ToggleButton::new(cx, key_mode.map(move |m: &usize| *m == n), move |cx| {
                        Label::new(cx, *label)
                    })
                    .on_press(move |_cx| {
                        key_mode.set(n);
                        if n == 0 {
                            edit_layer.set(0);
                        }
                        sh.set_key_mode_seeded(KeyMode::from_u8(n as u8));
                    });
                }
            })
            .class("ovsmp");

            // Upper/Lower edit-target toggle — hidden in Whole (editing layer A).
            let edit_vis = key_mode.map(|m: &usize| *m != 0);
            ButtonGroup::new(cx, move |cx| {
                for (n, label) in EDIT.iter().enumerate() {
                    ToggleButton::new(cx, edit_layer.map(move |l: &usize| *l == n), move |cx| {
                        Label::new(cx, *label)
                    })
                    .on_press(move |_cx| edit_layer.set(n));
                }
            })
            .class("ovsmp")
            .display(edit_vis);

            // Split point — shown only in Split. A horizontal slider over the
            // MIDI range with a note-name readout; writes the opaque split state.
            let split_vis = key_mode.map(|m: &usize| *m == 2);
            let sh_split = Arc::clone(shared);
            VStack::new(cx, move |cx| {
                Slider::new(cx, split.map(|n: &f32| *n / 127.0))
                    .width(Pixels(70.0))
                    .height(Pixels(14.0))
                    .on_change(move |_cx, v| {
                        let note = (v * 127.0).round().clamp(0.0, 127.0);
                        split.set(note);
                        sh_split.set_split_point(note as u8);
                    });
                Label::new(cx, split.map(|n: &f32| note_name(*n as u8)))
                    .class("ctl-value")
                    .height(Pixels(11.0));
            })
            .height(Auto)
            .vertical_gap(Pixels(2.0))
            .display(split_vis);
        })
        .height(Pixels(COL_H))
        .vertical_gap(Pixels(8.0))
        .alignment(Alignment::TopCenter);
    })
    .class("panel")
    .height(Pixels(PANEL_H))
    .padding(Pixels(5.0))
    .vertical_gap(Pixels(4.0));
}

/// Build one faceplate panel. Per-patch entries resolve to `layer`'s block;
/// `display` (when given) shows the panel only while it matches the edit layer,
/// so a per-patch panel is built once per layer and toggled by the Upper/Lower
/// switch without any structural rebuild.
fn panel_view(
    cx: &mut Context,
    title: &'static str,
    entries: &'static [Entry],
    layer: Layer,
    controls: &[Ctl],
    shared: &Arc<SharedParams>,
    display: Option<Memo<bool>>,
) {
    let handle = VStack::new(cx, |cx| {
        Label::new(cx, title)
            .class("panel-header")
            .width(Stretch(1.0))
            .height(Pixels(16.0))
            .alignment(Alignment::Center);
        HStack::new(cx, |cx| {
            for (id, short) in entries {
                let cid = resolve(*id, layer);
                let ctl = controls.iter().copied().find(|c| c.idx() == cid).unwrap();
                control_view(cx, ctl, shared, short);
            }
        })
        .height(Pixels(COL_H))
        .horizontal_gap(Pixels(6.0));
    })
    .class("panel")
    .height(Pixels(PANEL_H))
    .padding(Pixels(5.0))
    .vertical_gap(Pixels(4.0));
    if let Some(d) = display {
        handle.display(d);
    }
}

/// Polyline (in a `[0, 1]²` box, y down) approximating one cycle of a named
/// waveform, for the little icons drawn around a waveform selector knob. Returns
/// empty for labels that aren't waveforms (e.g. noise colours), which fall back to
/// text labels instead.
fn wave_points(label: &str) -> Vec<(f32, f32)> {
    match label {
        "Sine" => (0..=16)
            .map(|k| {
                let t = k as f32 / 16.0;
                (t, 0.5 - 0.38 * (t * std::f32::consts::TAU).sin())
            })
            .collect(),
        "Triangle" | "Tri" => vec![(0.0, 0.85), (0.5, 0.15), (1.0, 0.85)],
        // Rising ramp with a vertical reset (one and a bit cycles reads clearly small).
        "Saw" | "Saw+" => vec![(0.0, 0.85), (0.5, 0.15), (0.5, 0.85), (1.0, 0.15)],
        "Saw-" => vec![(0.0, 0.15), (0.5, 0.85), (0.5, 0.15), (1.0, 0.85)],
        "Pulse" | "Square" => vec![
            (0.0, 0.85),
            (0.0, 0.15),
            (0.5, 0.15),
            (0.5, 0.85),
            (1.0, 0.85),
        ],
        "S&H" => vec![
            (0.0, 0.6),
            (0.28, 0.6),
            (0.28, 0.2),
            (0.56, 0.2),
            (0.56, 0.8),
            (0.82, 0.8),
            (0.82, 0.45),
            (1.0, 0.45),
        ],
        _ => Vec::new(),
    }
}

/// A small waveform icon, stroked in the view's current `color` so a `.active`
/// class can light it up. Used as a glyph "label" around a waveform selector knob.
struct WaveGlyph {
    label: &'static str,
}

impl WaveGlyph {
    fn new<'a>(cx: &'a mut Context, label: &'static str) -> Handle<'a, Self> {
        Self { label }.build(cx, |_| {})
    }
}

impl View for WaveGlyph {
    fn element(&self) -> Option<&'static str> {
        Some("waveglyph")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let pts = wave_points(self.label);
        if pts.is_empty() {
            return;
        }
        let b = cx.bounds();
        let s = cx.scale_factor();
        let pad = 2.0 * s;
        let (w, h) = (b.w - 2.0 * pad, b.h - 2.0 * pad);
        let mut path = vg::PathBuilder::new();
        for (k, (t, y)) in pts.iter().enumerate() {
            let p = (b.x + pad + t * w, b.y + pad + y * h);
            if k == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
        let mut paint = vg::Paint::default();
        paint.set_color(cx.font_color());
        paint.set_stroke_width(1.3 * s);
        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_cap(vg::PaintCap::Round);
        paint.set_stroke_join(vg::PaintJoin::Round);
        paint.set_anti_alias(true);
        let path = path.detach();
        canvas.draw_path(&path, &paint);
    }
}

/// Cursor Y as a top offset (logical px) within the control cell, clamped to the
/// cell so the readout can't drift above it. Used to pin the value popup to the
/// point where the pointer entered/grabbed the control.
fn cursor_top(cx: &EventContext) -> f32 {
    let cell_y = cx.cache.get_bounds(cx.parent()).y;
    (((cx.mouse().cursor_y - cell_y) / cx.scale_factor()) - 8.0).max(0.0)
}

/// Floating value readout shown over a fader/knob while it is hovered or being
/// dragged. Absolutely positioned so it never reserves layout space, rendered only
/// while `show` is set, and pinned to `posy` (the cursor Y at hover/grab) so it
/// sits beside the pointer rather than over the control's label. Non-hoverable so
/// it doesn't steal the pointer and make the control flicker. The faceplate's
/// overflow stays visible so the readout can spill past the narrow control cell.
fn value_popup<T: ToStringLocalized + 'static>(
    cx: &mut Context,
    text: impl Res<T> + Clone + 'static,
    show: SyncSignal<bool>,
    posy: SyncSignal<f32>,
    x_off: f32,
) {
    Label::new(cx, text)
        .class("value-pop")
        .position_type(PositionType::Absolute)
        .top(posy.map(|y: &f32| Pixels(*y)))
        .left(Stretch(1.0))
        .right(Stretch(1.0))
        .width(Auto)
        .height(Auto)
        // Nudge sideways (faders) so the readout sits beside the thumb rather than
        // on top of it, keeping the thumb visible while dragging.
        .translate((Pixels(x_off), Pixels(0.0)))
        .z_index(100)
        .hoverable(false)
        .display(show);
}

fn control_view(cx: &mut Context, ctl: Ctl, shared: &Arc<SharedParams>, short: &'static str) {
    VStack::new(cx, |cx| {
        Label::new(cx, short)
            .class("ctl-label")
            .height(Pixels(11.0));
        match ctl {
            Ctl::Fader(i, sig) => {
                let (hover, drag, show, posy) = (
                    SyncSignal::new(false),
                    SyncSignal::new(false),
                    SyncSignal::new(false),
                    SyncSignal::new(0.0f32),
                );
                let (sh_set, sh_down, sh_up) =
                    (Arc::clone(shared), Arc::clone(shared), Arc::clone(shared));
                Slider::new(cx, sig)
                    .vertical(true)
                    .class("fader")
                    .width(Pixels(16.0))
                    .height(Pixels(FADER_H))
                    .on_change(move |_cx, v| {
                        sig.set(v);
                        sh_set.set(i, fader_from_ui(i, v));
                    })
                    .on_over(move |cx| {
                        posy.set(cursor_top(cx));
                        hover.set(true);
                        show.set(true);
                    })
                    .on_over_out(move |_cx| {
                        hover.set(false);
                        show.set(drag.get());
                    })
                    .on_mouse_down(move |cx, _btn| {
                        posy.set(cursor_top(cx));
                        drag.set(true);
                        show.set(true);
                        sh_down.set_gesture(i, true);
                    })
                    .on_mouse_up(move |_cx, _btn| {
                        drag.set(false);
                        show.set(hover.get());
                        sh_up.set_gesture(i, false);
                    });
                // A synced LFO rate reads as a musical subdivision; otherwise the
                // descriptor's own display (Hz, st, …). `sync_partner` is `None`
                // for every non-rate fader, so this collapses to the plain path.
                let sh_pop = Arc::clone(shared);
                value_popup(
                    cx,
                    sig.map(move |n: &f32| {
                        let plain = fader_from_ui(i, *n);
                        let desc = desc_for_clap_id(i).unwrap();
                        if let Some(sid) = sync_partner(i) {
                            if sh_pop.get(sid) >= 0.5 {
                                let norm = desc.to_normalized(plain);
                                return vxn_engine::sync::SUBDIVISIONS
                                    [vxn_engine::sync::index_from_norm(norm)]
                                .label
                                .to_string();
                            }
                        }
                        desc.display(plain)
                    }),
                    show,
                    posy,
                    22.0,
                );
            }
            Ctl::Rotary(i, sig) => {
                let cnt = match desc_for_clap_id(i).unwrap().kind {
                    ParamKind::Enum { variants } => variants.len(),
                    _ => 1,
                };
                let snap = move |n: f32| {
                    if cnt > 1 {
                        (n * (cnt - 1) as f32).round()
                    } else {
                        0.0
                    }
                };
                let default_norm = desc_for_clap_id(i)
                    .unwrap()
                    .to_normalized(desc_for_clap_id(i).unwrap().default);
                let variants = match desc_for_clap_id(i).unwrap().kind {
                    ParamKind::Enum { variants } => variants,
                    _ => &[][..],
                };
                // Waveform selectors get drawn glyphs around the arc; other enums
                // (e.g. noise colour) get small text labels at the same positions.
                let use_glyphs =
                    !variants.is_empty() && variants.iter().all(|l| !wave_points(l).is_empty());
                let (hover, drag, show, posy) = (
                    SyncSignal::new(false),
                    SyncSignal::new(false),
                    SyncSignal::new(false),
                    SyncSignal::new(0.0f32),
                );
                let (sh_set, sh_down, sh_up) =
                    (Arc::clone(shared), Arc::clone(shared), Arc::clone(shared));
                // Dial: knob centred, variant glyphs/labels arranged around its
                // 300° sweep (value 0..1 -> -150°..+150°, gap at the bottom). The
                // popup lives here too so its cursor-pinned offset shares the knob's
                // coordinate space.
                ZStack::new(cx, move |cx| {
                    const C: f32 = DIAL / 2.0;
                    const R: f32 = 25.0;
                    for (n, label) in variants.iter().enumerate() {
                        let value = if cnt > 1 {
                            n as f32 / (cnt - 1) as f32
                        } else {
                            0.5
                        };
                        let theta = (value * 300.0 - 150.0).to_radians();
                        let active = sig.map(move |v: &f32| {
                            cnt > 1 && (*v * (cnt - 1) as f32).round() as usize == n
                        });
                        if use_glyphs {
                            const G: f32 = 14.0;
                            WaveGlyph::new(cx, label)
                                .class("wave-glyph")
                                .toggle_class("active", active)
                                .position_type(PositionType::Absolute)
                                .left(Pixels(C + R * theta.sin() - G / 2.0))
                                .top(Pixels(C - R * theta.cos() - G / 2.0))
                                .width(Pixels(G))
                                .height(Pixels(G))
                                .hoverable(false);
                        } else {
                            const GW: f32 = 24.0;
                            const GH: f32 = 10.0;
                            Label::new(cx, *label)
                                .class("wave-txt")
                                .toggle_class("active", active)
                                .position_type(PositionType::Absolute)
                                .left(Pixels(C + R * theta.sin() - GW / 2.0))
                                .top(Pixels(C - R * theta.cos() - GH / 2.0))
                                .width(Pixels(GW))
                                .height(Pixels(GH))
                                .alignment(Alignment::Center)
                                .hoverable(false);
                        }
                    }
                    Knob::new(cx, default_norm, sig, false)
                        .on_change(move |_cx, v| {
                            // Snap to the nearest variant.
                            let idx = snap(v);
                            sig.set(if cnt > 1 { idx / (cnt - 1) as f32 } else { 0.0 });
                            sh_set.set(i, idx);
                        })
                        .on_over(move |cx| {
                            posy.set(cursor_top(cx));
                            hover.set(true);
                            show.set(true);
                        })
                        .on_over_out(move |_cx| {
                            hover.set(false);
                            show.set(drag.get());
                        })
                        .on_mouse_down(move |cx, _btn| {
                            posy.set(cursor_top(cx));
                            drag.set(true);
                            show.set(true);
                            sh_down.set_gesture(i, true);
                        })
                        .on_mouse_up(move |_cx, _btn| {
                            drag.set(false);
                            show.set(hover.get());
                            sh_up.set_gesture(i, false);
                        })
                        .size(Pixels(26.0));
                    value_popup(
                        cx,
                        sig.map(move |n: &f32| desc_for_clap_id(i).unwrap().display(snap(*n))),
                        show,
                        posy,
                        0.0,
                    );
                })
                .size(Pixels(DIAL))
                .alignment(Alignment::Center);
            }
            Ctl::Switch(i, sig) => {
                let sh = Arc::clone(shared);
                Switch::new(cx, sig).class("vswitch").on_toggle(move |_cx| {
                    let on = !sig.get();
                    sig.set(on);
                    sh.set(i, if on { 1.0 } else { 0.0 });
                });
                Label::new(
                    cx,
                    sig.map(move |b: &bool| {
                        desc_for_clap_id(i)
                            .unwrap()
                            .display(if *b { 1.0 } else { 0.0 })
                    }),
                )
                .class("ctl-value")
                .height(Pixels(11.0));
            }
            Ctl::Buttons(i, sig) => {
                let variants = match desc_for_clap_id(i).unwrap().kind {
                    ParamKind::Enum { variants } => variants,
                    _ => &[],
                };
                let shared = Arc::clone(shared);
                ButtonGroup::new(cx, move |cx| {
                    for (n, label) in variants.iter().enumerate() {
                        let sh = Arc::clone(&shared);
                        ToggleButton::new(
                            cx,
                            sig.map(move |s: &Option<usize>| *s == Some(n)),
                            move |cx| Label::new(cx, *label),
                        )
                        .on_press(move |_cx| {
                            sig.set(Some(n));
                            sh.set(i, n as f32);
                        });
                    }
                })
                .class("ovsmp");
            }
            Ctl::Select(i, sig) => {
                let variants = match desc_for_clap_id(i).unwrap().kind {
                    ParamKind::Enum { variants } => variants,
                    _ => &[],
                };
                let options = Signal::new(
                    variants
                        .iter()
                        .copied()
                        .map(Localized::new)
                        .collect::<Vec<_>>(),
                );
                let sh = Arc::clone(shared);
                Select::new(cx, options, sig, true)
                    .on_select(move |_cx, choice| {
                        sig.set(Some(choice));
                        sh.set(i, choice as f32);
                    })
                    .width(Pixels(62.0));
            }
        }
    })
    .height(Pixels(COL_H))
    .vertical_gap(Pixels(8.0))
    .alignment(Alignment::TopCenter);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repoints_per_patch_entries_per_layer() {
        // A per-patch entry (baked as the Upper id) re-points to the edit layer.
        let upper = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        assert_eq!(resolve(upper, Layer::Upper), upper);
        assert_eq!(
            resolve(upper, Layer::Lower),
            patch_clap_id(Layer::Lower, PatchParam::Cutoff)
        );
        // A global entry is fixed regardless of the edit layer.
        let vol = global_clap_id(GlobalParam::MasterVolume);
        assert_eq!(resolve(vol, Layer::Upper), vol);
        assert_eq!(resolve(vol, Layer::Lower), vol);
    }

    #[test]
    fn layer_dependence_classifies_panels() {
        let patch: &[Entry] = &[(patch_clap_id(Layer::Upper, PatchParam::Cutoff), "C")];
        let global: &[Entry] = &[(global_clap_id(GlobalParam::MasterVolume), "V")];
        assert!(is_layer_dependent(patch));
        assert!(!is_layer_dependent(global));
    }

    #[test]
    fn sync_partner_pairs_rate_with_its_toggle() {
        // LFO 1 rate ↔ sync on the same layer.
        for layer in Layer::ALL {
            assert_eq!(
                sync_partner(patch_clap_id(layer, PatchParam::LfoRate)),
                Some(patch_clap_id(layer, PatchParam::LfoSync))
            );
        }
        // LFO 2 rate ↔ sync, both global.
        assert_eq!(
            sync_partner(global_clap_id(GlobalParam::Lfo2Rate)),
            Some(global_clap_id(GlobalParam::Lfo2Sync))
        );
        // Non-rate faders have no sync partner.
        assert_eq!(sync_partner(patch_clap_id(Layer::Upper, PatchParam::Cutoff)), None);
        assert_eq!(sync_partner(global_clap_id(GlobalParam::MasterVolume)), None);
    }

    #[test]
    fn lfo_pitch_fader_is_curved_and_narrowed() {
        let id = patch_clap_id(Layer::Upper, PatchParam::LfoPitch);
        // Whole semitone at the top, silent at the bottom.
        assert!((fader_from_ui(id, 1.0) - 1.0).abs() < 1e-4);
        assert!(fader_from_ui(id, 0.0).abs() < 1e-6);
        // Midpoint lands at ~0.2 st (the subtle-vibrato sweet spot).
        assert!((fader_from_ui(id, 0.5) - 0.2).abs() < 1e-3);
        // Round-trips within the narrowed range.
        for n in [0.1, 0.5, 0.9, 1.0] {
            assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
        }
        // LFO 2's pitch route shares the curve.
        let id2 = global_clap_id(GlobalParam::MasterTune); // sanity: not curved
        assert!(!is_lfo_pitch(id2));
        assert!(is_lfo_pitch(patch_clap_id(Layer::Lower, PatchParam::Lfo2Pitch)));
    }

    #[test]
    fn adsr_time_fader_anchors_and_round_trips() {
        for p in [
            PatchParam::Env1Attack,
            PatchParam::Env1Decay,
            PatchParam::Env1Release,
            PatchParam::Env2Attack,
            PatchParam::Env2Decay,
            PatchParam::Env2Release,
        ] {
            let id = patch_clap_id(Layer::Upper, p);
            assert!(is_adsr_time(id));
            assert!(fader_from_ui(id, 0.0).abs() < 1e-4); // ~0 s
            assert!((fader_from_ui(id, 0.5) - 1.0).abs() < 1e-3); // midpoint = 1 s
            assert!((fader_from_ui(id, 1.0) - 10.0).abs() < 1e-3); // top = 10 s
            for n in [0.2, 0.5, 0.8, 1.0] {
                assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
            }
        }
        // Sustain is a level, not a time — stays linear.
        assert!(!is_adsr_time(patch_clap_id(Layer::Upper, PatchParam::Env1Sustain)));
    }

    #[test]
    fn note_names_are_correct() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4"); // A440
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }
}
