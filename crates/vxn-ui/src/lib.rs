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
//! Modulation is the fixed-route model (ADR 0004 §4): the Pitch / PWM channels
//! each carry an LFO source selector + depth and an Env source selector + depth
//! (dropdowns + faders), on the **Pitch Mod** / **PWM Mod** panels; the wide
//! osc-2 pitch route lives on **Cross Mod**. The Cutoff route (**Filter Mod**)
//! has fixed sources (E006) — velocity, LFO 1, LFO 2 and Env 1 each get their
//! own depth fader, no selectors. The **Mod Wheel** panel sits alongside. Mixer
//! carries the osc1/osc2/ring levels; the **Voice** panel surfaces the per-layer
//! assign-mode / unison / glide params (0023).
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
    AssignMode, CrossModType, DEFAULT_SPLIT_POINT, GlobalParam, KeyMode, Layer, ParamKind,
    ParamRef, PatchParam, SharedParams, TOTAL_PARAMS, desc_for_clap_id, global_clap_id, param_ref,
    patch_clap_id,
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

/// Split-slider MIDI range: C0 (12) … C7 (96). Narrower than the full 0..127 so
/// the 84-semitone span fills the slider's travel and every semitone is easy to
/// select; the stored split point stays the raw MIDI note.
const SPLIT_MIN: f32 = 12.0;
const SPLIT_MAX: f32 = 96.0;

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

pub const EDITOR_WIDTH: u32 = 1024;
/// Four panel rows: (1) LFOs + oscillators + mixer, (2) envelopes + filter +
/// filter mod, (3) the per-osc mod routes + performance wheels, (4) voice +
/// master + keys + the two effects.
pub const EDITOR_HEIGHT: u32 = 734;

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
        DelayPingPong, DelaySync, DelayTime, Lfo2Rate, Lfo2Shape, Lfo2Sync, MasterTune,
        MasterVolume, Oversample,
    };
    use PatchParam::*;
    &[
        // Row 1 — modulation sources (the two LFOs) then the oscillators + mixer.
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
                // shape/rate/sync are global. It reaches the routes through the
                // per-channel {Off/LFO1/LFO2} source selectors, not its own cells.
                "LFO 2",
                &[
                    (g(Lfo2Shape), "Shape"),
                    (g(Lfo2Rate), "Rate"),
                    (g(Lfo2Sync), "Sync"),
                ],
            ),
            (
                "Osc 1",
                &[
                    (u(Osc1Wave), "Wave"),
                    (u(Osc1Octave), "Oct"),
                    (u(Osc1Coarse), "Semi"),
                    (u(Osc1Fine), "Fine"),
                    (u(Osc1PulseWidth), "PW"),
                ],
            ),
            (
                "Osc 2",
                &[
                    (u(Osc2Wave), "Wave"),
                    (u(Osc2Octave), "Oct"),
                    (u(Osc2Coarse), "Semi"),
                    (u(Osc2Fine), "Fine"),
                    (u(Osc2PulseWidth), "PW"),
                ],
            ),
            (
                // osc1/osc2/ring levels (ADR 0004 §6 / "Panel layout").
                "Mixer",
                &[
                    (u(Osc1Level), "Osc1"),
                    (u(Osc2Level), "Osc2"),
                    (u(RingLevel), "Ring"),
                ],
            ),
        ],
        // Row 2 — envelopes, filter, filter mod.
        &[
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
            (
                "Filter",
                &[
                    (u(HpfCutoff), "HPF"),
                    (u(Cutoff), "Cutoff"),
                    (u(Resonance), "Reso"),
                    (u(Drive), "Drive"),
                    (u(FilterKeyTrack), "KeyTrk"),
                    (u(FilterVariant), "Type"),
                ],
            ),
            (
                // Cutoff route (E006): four fixed-source depths into cutoff —
                // velocity, both LFOs and Env 1 (Env→cutoff is always Env 1). No
                // source selectors, so this is a plain four-fader row.
                "Filter Mod",
                &[
                    (u(VelCutoffDepth), "Vel"),
                    (u(CutoffLfo1Depth), "LFO1"),
                    (u(CutoffLfo2Depth), "LFO2"),
                    (u(CutoffEnvDepth), "Env1"),
                ],
            ),
        ],
        // Row 3 — the per-osc mod routes + performance wheels.
        &[
            (
                // Osc Mod split three ways (ADR 0004 §4 routes), labels simplified
                // since the panel header now carries the destination.
                "Pitch Mod",
                &[
                    (u(PitchLfoSrc), "LFO"),
                    (u(PitchLfoDepth), "LFO.D"),
                    (u(PitchEnvSrc), "Env"),
                    (u(PitchEnvDepth), "Env.D"),
                ],
            ),
            (
                "PWM Mod",
                &[
                    (u(PwmLfoSrc), "LFO"),
                    (u(PwmLfoDepth), "LFO.D"),
                    (u(PwmEnvSrc), "Env"),
                    (u(PwmEnvDepth), "Env.D"),
                ],
            ),
            (
                // Cross-mod type {Off/Sync/PM} + amount, alongside the wide
                // osc2-only pitch route (octave range) that drives the sweep. Each
                // selector sits beside its depth fader; the fader greys out while
                // its selector is Off. Custom layout — see `cross_mod_panel`.
                "Cross Mod",
                &[
                    (u(CrossModType), "Type"),
                    (u(CrossModAmount), "Amt"),
                    (u(Osc2PitchEnvSrc), "Src"),
                    (u(Osc2PitchEnvDepth), "Mod"),
                ],
            ),
            (
                "Mod Wheel",
                &[
                    (u(ModWheelPwm), "PWM"),
                    (u(ModWheelCutoff), "Cutoff"),
                    (u(ModWheelReso), "Reso"),
                    (u(ModWheelOsc2Pitch), "O2 Pitch"),
                ],
            ),
            (
                // Pitch-bend wheel range (vibrato-scaled, both oscillators), sat
                // beside the mod wheel as the other performance-wheel control. A
                // single fader, so the panel is narrowed (see `panel_view`) and
                // titled "Bend" to free horizontal space in the row.
                "Bend",
                &[(u(PitchWheelDepth), "Range")],
            ),
        ],
        // Row 4 — keys, voice, the two effects, then global master.
        &[
            // Keys leads the row. It has no plain entries — `build_editor`
            // special-cases this title to `keys_panel`, since the mode/split write
            // opaque (non-param) state.
            ("Keys", &[]),
            (
                // Per-layer voice assignment + glide (E003): assign mode, unison
                // detune, glide on/off + time. Not in ADR 0004's panel list, but
                // these are live automatable params; the faceplate surfaces every
                // such param (0023 acceptance), so they get a dedicated panel.
                "Voice",
                &[
                    (u(AssignMode), "Assign"),
                    (u(UnisonDetune), "Detune"),
                    // No glide on/off: the time fader is the whole control (0 = off).
                    (u(PortamentoTime), "Glide"),
                ],
            ),
            (
                // The On bool is lifted into the panel header (left of the title);
                // `panel_view` drops it from the cell row. See `header_switch`.
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
                    (g(DelaySync), "Sync"),
                    (g(DelayFeedback), "FB"),
                    (g(DelayMix), "Mix"),
                    (g(DelayPingPong), "Ping-Pong"),
                ],
            ),
            (
                "Master",
                &[
                    (g(MasterTune), "Tune"),
                    (g(MasterVolume), "Volume"),
                    (g(Oversample), "OvSmp"),
                ],
            ),
        ],
    ]
};

/// A modulation route as a faceplate column: a short column header, an optional
/// source-selector param (the `{Off/LFO/Env}` picker, `None` for a fixed source
/// like velocity or the pitch wheel), and the depth fader param. Rendered as the
/// depth fader with the selector boxes stacked directly beneath it — pairing the
/// "where from" and "how much" of one route in a single column.
type Route = (&'static str, Option<usize>, usize);

const PITCH_MOD_ROUTES: &[Route] = {
    use PatchParam::*;
    &[
        ("LFO", Some(u(PitchLfoSrc)), u(PitchLfoDepth)),
        ("Env", Some(u(PitchEnvSrc)), u(PitchEnvDepth)),
    ]
};

const PWM_MOD_ROUTES: &[Route] = {
    use PatchParam::*;
    &[
        ("LFO", Some(u(PwmLfoSrc)), u(PwmLfoDepth)),
        ("Env", Some(u(PwmEnvSrc)), u(PwmEnvDepth)),
    ]
};

/// The route-column table for a mod panel, or `None` for a panel laid out as a
/// plain row of control cells. Filter Mod is *not* here (E006): its sources are
/// fixed, so it renders as a plain four-fader row.
fn routes_for(title: &str) -> Option<&'static [Route]> {
    match title {
        "Pitch Mod" => Some(PITCH_MOD_ROUTES),
        "PWM Mod" => Some(PWM_MOD_ROUTES),
        _ => None,
    }
}

/// Stylesheet: dark faceplate, orange panel headers, small text.
const STYLE: &str = r#"
:root { background-color: #2b2b2b; font-family: "IBM Plex Sans Condensed Medium"; }
label { font-size: 10; color: #d6d6d6; }
.panel { background-color: #1c1c1c; border-width: 1px; border-color: #0e0e0e; corner-radius: 4px; }
.panel-header { background-color: #a7cfe2; color: #141414; corner-radius: 2px; font-size: 10; }
.banner { background-color: #1c1c1c; border-width: 1px; border-color: #0e0e0e; corner-radius: 4px; color: #a7cfe2; font-size: 16; letter-spacing: 3px; }
.ctl-label { font-size: 8; color: #aeaeae; }
.ctl-value { font-size: 8; color: #d9701b; }
.tg-list { gap: 1px; }
/* Compact rows: vizia's default toggle-button is 32px tall, which overflows a
   4-row picker and towers over the faders. 24px lets a 4-row list fit and a
   3-row list match the fader height (so they line up). */
.tg-row { height: 24px; background-color: transparent; border-width: 0px; padding: 0px; }
.tg-row:hover { background-color: transparent; }
.tg-row:checked { background-color: transparent; }
.tg-row:checked:hover { background-color: transparent; }
.tg-box { width: 9px; height: 9px; background-color: #4a4a4a; border-width: 1px; border-color: #8a8a8a; corner-radius: 2px; }
.tg-row:hover .tg-box { border-color: #c4c4c4; }
.tg-row:checked .tg-box { background-color: #1f9cff; border-color: #84cdff; shadow: 0px 0px 6px #36b3ff; }
.tg-lbl { font-size: 7; color: #9a9a9a; }
.tg-row:checked .tg-lbl { color: #ececec; }
.value-pop { background-color: #0e0e0e; border-width: 1px; border-color: #d9701b; corner-radius: 3px; padding-left: 4px; padding-right: 4px; font-size: 9; color: #f6f6f6; }
.reset-btn { height: 14px; background-color: #333333; border-width: 1px; border-color: #555555; corner-radius: 2px; }
.reset-btn:hover { background-color: #3a3a3a; border-color: #c4c4c4; }
.reset-btn .tg-lbl { color: #b0b0b0; }
.reset-btn:hover .tg-lbl { color: #ececec; }
.fader .track { background-color: #555555; width: 6px; corner-radius: 2px; }
.fader .range { background-color: #3a86cc; corner-radius: 2px; }
.fader .thumb { background-color: #e8e8e8; border-width: 1px; border-color: #141414; corner-radius: 1px; width: 20px; height: 8px; }
.wave-glyph { color: #888888; }
.wave-glyph.active { color: #a7cfe2; }
.wave-txt { font-size: 8; color: #888888; }
.wave-txt.active { color: #a7cfe2; }
.dimmed { opacity: 0.35; }
"#;

/// Fader travel, sized to match a 3-row selector list (3 × the 24px `.tg-row` +
/// gaps) so faders and the mod-section pickers line up at the bottom.
const FADER_H: f32 = 74.0;
const COL_H: f32 = 120.0;
const PANEL_H: f32 = 156.0;

/// The faceplate is all-caps: uppercase a label's text at render (the source
/// strings stay mixed-case for matching / readability).
fn up(s: &str) -> String {
    s.to_uppercase()
}
/// Square area framing a selector knob, sized to fit the variant glyphs/labels
/// arranged around its arc.
const DIAL: f32 = 62.0;

/// Twin's useful detune ceiling (cents): the `UnisonDetune` fader tops out here
/// in Twin (vs the descriptor's full 50 ct, used in Unison), and switching *into*
/// Twin clamps the stored value to it.
const TWIN_DETUNE_CT: f32 = 20.0;

/// Display order of the assign-mode picker rows: Poly, Twin, Unison, Solo — i.e.
/// `AssignMode` indices `[Poly, Twin, Unison, Solo]`. View order only; each row
/// still writes its variant's own index.
const ASSIGN_DISPLAY_ORDER: [usize; 4] = [
    AssignMode::Poly as usize,
    AssignMode::Twin as usize,
    AssignMode::Unison as usize,
    AssignMode::Solo as usize,
];

/// Plain value → fader position `[0, 1]`. The whole mapping — range and any
/// exponential taper — lives on the parameter descriptor ([`ParamDesc::to_fader`],
/// driven by its [`Taper`]), so the editor fader, the descriptor's clamp on every
/// write and the host's normalized range all agree from one definition. Unknown
/// ids (none in practice) fall back to the bottom.
fn fader_to_ui(idx: usize, value: f32) -> f32 {
    desc_for_clap_id(idx).map_or(0.0, |d| d.to_fader(value))
}

/// Fader position `[0, 1]` → plain value (inverse of [`fader_to_ui`]).
fn fader_from_ui(idx: usize, n: f32) -> f32 {
    desc_for_clap_id(idx).map_or(n.clamp(0.0, 1.0), |d| d.from_fader(n))
}

/// Mode-dependent fader top for `UnisonDetune`: its *useful* detune differs by
/// the layer's assign mode — a wide 50 ct in **Unison** (a lush chorus stack) vs
/// a subtle 20 ct in **Twin** (a 2-voice spread). The stored value stays plain
/// cents (descriptor max 50); only the fader's full-travel meaning changes, so
/// the same control reads ergonomically in either mode. `None` for every other
/// fader (they use the descriptor's own range + taper via `fader_to_ui`).
fn detune_top(idx: usize, shared: &SharedParams) -> Option<f32> {
    match param_ref(idx) {
        Some(ParamRef::Patch(layer, PatchParam::UnisonDetune)) => {
            let mode = shared
                .get(patch_clap_id(layer, PatchParam::AssignMode))
                .round() as usize;
            Some(if mode == AssignMode::Unison as usize {
                50.0
            } else {
                TWIN_DETUNE_CT
            })
        }
        _ => None,
    }
}

/// On switching a layer to **Twin**, clamp that layer's `UnisonDetune` down to
/// Twin's ceiling ([`TWIN_DETUNE_CT`]) — a wide value dialled in for Unison's
/// 50 ct range would otherwise carry over as an out-of-character spread. No-op
/// for any other param or target mode; the detune fader follows the store on the
/// next idle poll.
fn clamp_detune_on_twin(idx: usize, variant: usize, shared: &SharedParams) {
    if let Some(ParamRef::Patch(layer, PatchParam::AssignMode)) = param_ref(idx) {
        if variant == AssignMode::Twin as usize {
            let dt = patch_clap_id(layer, PatchParam::UnisonDetune);
            shared.set(dt, shared.get(dt).min(TWIN_DETUNE_CT));
        }
    }
}

/// [`fader_to_ui`] with the live `UnisonDetune` mode scaling applied; identical to
/// the plain mapping for every other fader.
fn fader_to_ui_dyn(idx: usize, value: f32, shared: &SharedParams) -> f32 {
    match detune_top(idx, shared) {
        Some(top) if top > 0.0 => (value / top).clamp(0.0, 1.0),
        _ => fader_to_ui(idx, value),
    }
}

/// [`fader_from_ui`] with the live `UnisonDetune` mode scaling applied.
fn fader_from_ui_dyn(idx: usize, n: f32, shared: &SharedParams) -> f32 {
    match detune_top(idx, shared) {
        Some(top) => n.clamp(0.0, 1.0) * top,
        None => fader_from_ui(idx, n),
    }
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
        // The delay time knob host-syncs the same way (E006): with sync on its
        // position reads as a musical subdivision instead of seconds.
        Some(ParamRef::Global(GlobalParam::DelayTime)) => {
            Some(global_clap_id(GlobalParam::DelaySync))
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
    // Segmented button groups: Oversample, the three-way cross-mod type
    // {Off/Sync/FM}, and the Poly/Unison assign mode — all read as labelled mode
    // pickers rather than dials/switches.
    let is_buttons = matches!(
        param_ref(i),
        Some(ParamRef::Global(GlobalParam::Oversample))
            | Some(ParamRef::Patch(
                _,
                PatchParam::CrossModType | PatchParam::AssignMode
            ))
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
        _ => Ctl::Fader(i, SyncSignal::new(fader_to_ui_dyn(i, shared.get(i), shared))),
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
                        // `_dyn` so the detune fader re-maps when the assign mode
                        // changes (its useful top is mode-dependent).
                        let n = fader_to_ui_dyn(i, self.shared.get(i), &self.shared);
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
///
/// `scale_override` pins the HiDPI factor when the caller already knows the true
/// backing scale (macOS reads it from the parent `NSView`'s window). This
/// sidesteps `WindowScalePolicy::SystemScaleFactor`, whose 1.25 placeholder is
/// only corrected by a `viewDidChangeBackingProperties` → `Resized` event — and
/// on a 1× display that event never fires (the backing scale never changes from
/// what baseview already recorded), so the editor stays stuck at 1.25 and
/// renders ~1.25× oversized with mouse hit-testing offset to match.
///
/// Because a pinned `ScaleFactor` makes vizia_baseview create the initial Skia
/// surface at the *unscaled* logical size (it only rebuilds on a `Resized`,
/// which a self-driven macOS resize doesn't emit), the idle callback emits one
/// `SetUserScale(1.0)` on the first tick to force `apply_user_scale` to recreate
/// the surface at `inner_size × scale` — required for the 2× (Retina) case.
pub fn open_editor(
    parent: *mut c_void,
    shared: Arc<SharedParams>,
    scale_override: Option<f64>,
) -> EditorHandle {
    let parent = ParentWindow(parent);

    // Per-open idle tick counter (interior-mutable so the `Fn` idle closure can
    // bump it). Process-static would leak across reopens and skip the one-time
    // surface rebuild on the second window.
    let tick = std::cell::Cell::new(0u32);

    let mut app = Application::new(move |cx| build_editor(cx, Arc::clone(&shared)))
        .on_idle(move |cx| {
            cx.emit(PollAutomation);
            let n = tick.get();
            tick.set(n.saturating_add(1));
            if n == 0 {
                // Force the one-time surface rebuild at the correct physical size.
                cx.emit(WindowEvent::SetUserScale(1.0));
            }
        })
        .inner_size((EDITOR_WIDTH, EDITOR_HEIGHT))
        .title("VXN1");

    if let Some(scale) = scale_override.filter(|s| *s > 0.0) {
        app = app.with_scale_policy(WindowScalePolicy::ScaleFactor(scale));
    }

    app.open_parented(&parent)
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

    ScrollView::new(cx, move |cx| {
        VStack::new(cx, |cx| {
            // Branding banner across the top, pushing the panel rows down.
            Label::new(cx, "VULPUS LABS - VXN-1")
                .class("banner")
                .width(Stretch(1.0))
                .height(Pixels(26.0))
                .alignment(Alignment::Center);
            for row in ROWS.iter() {
                HStack::new(cx, |cx| {
                    for (title, entries) in *row {
                        if *title == "Keys" {
                            // Placeholder entry: the key-mode panel writes opaque
                            // (non-param) state, so it's built directly rather than
                            // from `entries`. Its row slot fixes its position.
                            keys_panel(cx, &shared, edit_layer, key_mode, split);
                        } else if is_layer_dependent(entries) {
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
                })
                .height(Pixels(PANEL_H))
                .horizontal_gap(Pixels(6.0));
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
        Label::new(cx, up("Keys"))
            .class("panel-header")
            .width(Stretch(1.0))
            .height(Pixels(16.0))
            .alignment(Alignment::Center);
        VStack::new(cx, move |cx| {
            // Key-mode selector on the left; the Upper/Lower edit toggle and (under
            // it) the split-point control stacked on the right. Both are hidden
            // until a multi-layer mode reveals them.
            let sh_mode = Arc::clone(shared);
            HStack::new(cx, move |cx| {
                // Mode list. Choosing Whole snaps the edit target back to Upper (the
                // toggle is hidden), so we never edit a hidden Lower.
                VStack::new(cx, move |cx| {
                    for (n, label) in MODES.iter().enumerate() {
                        let sh = Arc::clone(&sh_mode);
                        toggle_row(cx, label, key_mode.map(move |m: &usize| *m == n), move |_cx| {
                            key_mode.set(n);
                            if n == 0 {
                                edit_layer.set(0);
                            }
                            sh.set_key_mode_seeded(KeyMode::from_u8(n as u8));
                        });
                    }
                })
                .class("tg-list")
                .height(Auto);

                // Upper/Lower edit toggle (hidden in Whole).
                let edit_vis = key_mode.map(|m: &usize| *m != 0);
                VStack::new(cx, move |cx| {
                    for (n, label) in EDIT.iter().enumerate() {
                        toggle_row(cx, label, edit_layer.map(move |l: &usize| *l == n), move |_cx| {
                            edit_layer.set(n)
                        });
                    }
                })
                .class("tg-list")
                .height(Auto)
                .display(edit_vis);
            })
            .height(Auto)
            .horizontal_gap(Pixels(12.0))
            .alignment(Alignment::TopLeft);

            // Split point — shown only in Split, spanning the full panel width below
            // the mode/edit rows. A horizontal slider over the MIDI range; the note
            // name shows as a tooltip pinned to the pointer's X, hovering above the
            // slider, on hover or drag. Writes the opaque split state.
            let split_vis = key_mode.map(|m: &usize| *m == 2);
            let (hover, drag, show, posx) = (
                SyncSignal::new(false),
                SyncSignal::new(false),
                SyncSignal::new(false),
                SyncSignal::new(0.0f32),
            );
            let (sh_change, sh_dbl) = (Arc::clone(shared), Arc::clone(shared));
            VStack::new(cx, move |cx| {
                Slider::new(
                    cx,
                    split.map(|n: &f32| {
                        ((*n - SPLIT_MIN) / (SPLIT_MAX - SPLIT_MIN)).clamp(0.0, 1.0)
                    }),
                )
                    .width(Stretch(1.0))
                    .height(Pixels(14.0))
                    .cursor(CursorIcon::Hand)
                    .on_change(move |_cx, v| {
                        // Map the slider over C0..C7 only (a narrower span than the
                        // full MIDI range), so every semitone is easy to land on.
                        let note = (SPLIT_MIN + v * (SPLIT_MAX - SPLIT_MIN))
                            .round()
                            .clamp(SPLIT_MIN, SPLIT_MAX);
                        split.set(note);
                        sh_change.set_split_point(note as u8);
                    })
                    .on_double_click(move |_cx, _btn| {
                        split.set(DEFAULT_SPLIT_POINT as f32);
                        sh_dbl.set_split_point(DEFAULT_SPLIT_POINT);
                    })
                    .on_hover(move |cx| {
                        posx.set(cursor_left(cx));
                        hover.set(true);
                        show.set(true);
                    })
                    .on_hover_out(move |_cx| {
                        hover.set(false);
                        show.set(drag.get());
                    })
                    .on_mouse_down(move |cx, _btn| {
                        posx.set(cursor_left(cx));
                        drag.set(true);
                        show.set(true);
                    })
                    .on_mouse_up(move |_cx, _btn| {
                        drag.set(false);
                        show.set(hover.get());
                    });
                // Note-name tooltip, pinned above the slider at the pointer X.
                Label::new(cx, split.map(|n: &f32| note_name(*n as u8)))
                    .class("value-pop")
                    .position_type(PositionType::Absolute)
                    .left(posx.map(|x: &f32| Pixels(*x)))
                    .top(Pixels(-16.0))
                    .width(Auto)
                    .height(Auto)
                    .translate((Pixels(-10.0), Pixels(0.0)))
                    .z_index(100)
                    .hoverable(false)
                    .display(show);
            })
            .width(Stretch(1.0))
            .height(Auto)
            .display(split_vis);

            // Reset the patch(es) currently being edited to plain defaults. In
            // Whole the two layers share one patch, so reset both; in Dual/Split
            // reset only the layer the edit toggle points at. Globals, key mode and
            // split point are setup state, left untouched. The `on_idle` poll
            // re-syncs every control signal from the store afterwards.
            let sh_reset = Arc::clone(shared);
            Button::new(cx, |cx| Label::new(cx, up("Reset")).class("tg-lbl"))
                .class("reset-btn")
                .width(Stretch(1.0))
                .cursor(CursorIcon::Hand)
                .on_press(move |_cx| {
                    if key_mode.get() == 0 {
                        sh_reset.reset_patch_to_defaults(Layer::Upper);
                        sh_reset.reset_patch_to_defaults(Layer::Lower);
                    } else {
                        let layer =
                            if edit_layer.get() == 0 { Layer::Upper } else { Layer::Lower };
                        sh_reset.reset_patch_to_defaults(layer);
                    }
                });
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
    // Chorus / Delay lift their leading On bool into the header (a toggle box on
    // the left of the title bar); the cell row then skips that first entry.
    let header_switch = matches!(title, "Chorus" | "Delay");
    let handle = VStack::new(cx, |cx| {
        if header_switch {
            let on = controls
                .iter()
                .copied()
                .find(|c| c.idx() == resolve(entries[0].0, layer));
            HStack::new(cx, |cx| {
                if let Some(Ctl::Switch(i, sig)) = on {
                    let sh = Arc::clone(shared);
                    toggle_row(cx, "", sig, move |_cx| {
                        let v = !sig.get();
                        sig.set(v);
                        sh.set(i, if v { 1.0 } else { 0.0 });
                    });
                }
                Label::new(cx, up(title))
                    .class("panel-header")
                    .width(Stretch(1.0))
                    .height(Pixels(16.0))
                    .alignment(Alignment::Center);
            })
            // Orange title-bar bg spans the whole header, so the toggle box sits on
            // the same colour as the title rather than the dark panel.
            .class("panel-header")
            .height(Pixels(16.0))
            .horizontal_gap(Pixels(4.0))
            .padding_left(Pixels(3.0))
            .alignment(Alignment::Left);
        } else {
            Label::new(cx, up(title))
                .class("panel-header")
                .width(Stretch(1.0))
                .height(Pixels(16.0))
                .alignment(Alignment::Center);
        }
        HStack::new(cx, |cx| {
            // Cross Mod is a custom pairing (selector beside fader, grey-when-Off);
            // the other mod panels lay out by route (depth fader + source selector
            // beneath); every other panel is a plain row of control cells.
            if title == "Cross Mod" {
                cross_mod_panel(cx, layer, controls, shared);
            } else if title == "LFO 1" {
                lfo1_cells(cx, layer, controls, shared);
            } else if let Some(routes) = routes_for(title) {
                for (head, src, depth) in routes {
                    mod_route_view(cx, head, *src, *depth, layer, controls, shared);
                }
            } else {
                // The On bool sits in the header; strip controls drop to the bottom
                // strip (below). Everything else is a column in the main row.
                let cells = if header_switch { &entries[1..] } else { entries };
                for (id, short) in cells {
                    if in_bottom_strip(*id) {
                        continue;
                    }
                    let cid = resolve(*id, layer);
                    let ctl = controls.iter().copied().find(|c| c.idx() == cid).unwrap();
                    control_view(cx, ctl, shared, short);
                }
            }
        })
        .height(Pixels(COL_H))
        // Osc panels take a wider stretch share (below); keep their controls in a
        // tight centred group (rather than spread edge-to-edge) so the faders sit
        // close and the wave selector pulls in from the panel side.
        .horizontal_gap(if matches!(title, "Osc 1" | "Osc 2") {
            Pixels(8.0)
        } else {
            Pixels(4.0)
        })
        .alignment(if matches!(title, "Osc 1" | "Osc 2") {
            Alignment::Center
        } else {
            Alignment::TopLeft
        });

        // Bottom strip: selector/toggle controls relocated into the clearance
        // below the main row (frees a horizontal column up top). Absolutely placed
        // so it sits in that empty space without growing the panel.
        if entries.iter().any(|(id, _)| in_bottom_strip(*id)) {
            HStack::new(cx, |cx| {
                for (id, short) in entries {
                    if !in_bottom_strip(*id) {
                        continue;
                    }
                    let cid = resolve(*id, layer);
                    let ctl = controls.iter().copied().find(|c| c.idx() == cid).unwrap();
                    strip_cell(cx, ctl, short, shared);
                }
            })
            .position_type(PositionType::Absolute)
            .left(Pixels(8.0))
            .right(Stretch(1.0))
            .top(Stretch(1.0))
            .bottom(Pixels(7.0))
            .height(Auto)
            .horizontal_gap(Pixels(12.0));
        }
    })
    .class("panel")
    .height(Pixels(PANEL_H))
    .padding(Pixels(5.0))
    .vertical_gap(Pixels(4.0));
    // Per-panel width share. Panels otherwise stretch equally across a row; the
    // five-control Osc panels take a bigger share and the three-fader Mixer a
    // smaller one, and the single-fader Bend panel is pinned narrow.
    let handle = match title {
        "Bend" => handle.width(Pixels(54.0)),
        "Osc 1" | "Osc 2" => handle.width(Stretch(1.2)),
        "LFO 1" => handle.width(Stretch(1.2)),
        "Mixer" => handle.width(Stretch(0.8)),
        _ => handle,
    };
    if let Some(d) = display {
        handle.display(d);
    }
}

/// Polyline (in a `[0, 1]²` box, y down) approximating one cycle of a named
/// waveform, for the little icons drawn around a waveform selector knob. Returns
/// empty for labels that aren't waveforms (e.g. oversample labels), which fall
/// back to text labels instead.
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

/// Cursor X as a left offset (logical px) within the control's parent cell. Pins
/// a horizontal slider's hover readout to the pointer column (the X analogue of
/// [`cursor_top`]).
fn cursor_left(cx: &EventContext) -> f32 {
    let cell_x = cx.cache.get_bounds(cx.parent()).x;
    ((cx.mouse().cursor_x - cell_x) / cx.scale_factor()).max(0.0)
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

/// One row of a compact selector/toggle: a small grey indicator box that lights
/// red while active (driven by the host `ToggleButton`'s `:checked` state via the
/// stylesheet), with `label` text alongside. `label` is empty for a plain bool,
/// which shows just the box. `active` tracks the on state; `press` commits it.
fn toggle_row(
    cx: &mut Context,
    label: &'static str,
    active: impl Res<bool> + Copy + 'static,
    press: impl Fn(&mut EventContext) + Send + Sync + 'static,
) {
    ToggleButton::new(cx, active, move |cx| {
        HStack::new(cx, move |cx| {
            Element::new(cx).class("tg-box");
            if !label.is_empty() {
                Label::new(cx, up(label)).class("tg-lbl");
            }
        })
        .height(Auto)
        .horizontal_gap(Pixels(4.0))
        .alignment(Alignment::Left)
    })
    .class("tg-row")
    .cursor(CursorIcon::Hand)
    .on_press(press);
}

/// The vertical fader + its hover/drag value popup, without any label — shared by
/// a plain control cell and a mod-route column (where the column header labels it).
/// `disabled` (a build-time bool) blocks the slider's drag (vizia's `Slider`
/// guards on `cx.is_disabled()`). The Cross Mod depth faders pass this from inside
/// a [`Binding`] on their selector, so the column is rebuilt — and re-disabled or
/// re-enabled — whenever the selector leaves/returns to Off.
fn fader_body(
    cx: &mut Context,
    i: usize,
    sig: SyncSignal<f32>,
    shared: &Arc<SharedParams>,
    disabled: bool,
) {
    let (hover, drag, show, posy) = (
        SyncSignal::new(false),
        SyncSignal::new(false),
        SyncSignal::new(false),
        SyncSignal::new(0.0f32),
    );
    let (sh_set, sh_down, sh_up, sh_dbl) = (
        Arc::clone(shared),
        Arc::clone(shared),
        Arc::clone(shared),
        Arc::clone(shared),
    );
    let slider = Slider::new(cx, sig)
        .vertical(true)
        .class("fader")
        .cursor(CursorIcon::Hand)
        .width(Pixels(16.0))
        .height(Pixels(FADER_H))
        .on_change(move |_cx, v| {
            sig.set(v);
            sh_set.set(i, fader_from_ui_dyn(i, v, &sh_set));
        })
        // Double-click resets the fader to its parameter default (bracketed by a
        // gesture so the host records the jump as one edit).
        .on_double_click(move |_cx, _btn| {
            let d = desc_for_clap_id(i).map_or(0.0, |d| d.default);
            sig.set(fader_to_ui_dyn(i, d, &sh_dbl));
            sh_dbl.set_gesture(i, true);
            sh_dbl.set(i, d);
            sh_dbl.set_gesture(i, false);
        })
        .on_hover(move |cx| {
            posy.set(cursor_top(cx));
            hover.set(true);
            show.set(true);
        })
        .on_hover_out(move |_cx| {
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
    slider.disabled(disabled);
    // A synced LFO rate reads as a musical subdivision; otherwise the descriptor's
    // own display (Hz, st, …). `sync_partner` is `None` for every non-rate fader,
    // so this collapses to the plain path.
    let sh_pop = Arc::clone(shared);
    value_popup(
        cx,
        sig.map(move |n: &f32| {
            let plain = fader_from_ui_dyn(i, *n, &sh_pop);
            let desc = desc_for_clap_id(i).unwrap();
            if let Some(sid) = sync_partner(i) {
                if sh_pop.get(sid) >= 0.5 {
                    // Spread subdivisions linearly across the slider travel (the
                    // fader position), not the tapered Hz norm — even spacing, no
                    // midpoint skew. Matches the engine's `to_fader` resolve.
                    let pos = desc.to_fader(plain);
                    return vxn_engine::sync::SUBDIVISIONS[vxn_engine::sync::index_from_norm(pos)]
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

/// Selector/toggle controls that move out of the main control row into the
/// panel's bottom strip (freeing a horizontal column up top). Matched on the
/// typed param so it holds across both layers. The mod-route source selectors are
/// deliberately *not* here — they stay vertical beside their faders.
fn in_bottom_strip(idx: usize) -> bool {
    use PatchParam::{Env1Shape, Env2Shape, FilterKeyTrack, FilterVariant, Lfo1FreeRun, LfoSync};
    matches!(
        param_ref(idx),
        Some(ParamRef::Patch(
            _,
            LfoSync | Lfo1FreeRun | Env1Shape | Env2Shape | FilterVariant | FilterKeyTrack
        )) | Some(ParamRef::Global(
            GlobalParam::Lfo2Sync
                | GlobalParam::DelaySync
                | GlobalParam::DelayPingPong
                | GlobalParam::Oversample
        ))
    )
}

/// One bottom-strip control laid out **horizontally**: a plain bool is a single
/// labelled box; a small enum (Lin/Exp, Sharp/Smooth, the oversample modes) is a
/// row of exclusive labelled boxes. The vertical [`enum_list_body`] equivalent for
/// the strip.
fn strip_cell(cx: &mut Context, ctl: Ctl, short: &'static str, shared: &Arc<SharedParams>) {
    match ctl {
        Ctl::Switch(i, sig) => match desc_for_clap_id(i).unwrap().kind {
            // Two-state enum (Lin/Exp, Sharp/Smooth): both option boxes in a row.
            ParamKind::Enum { variants } => {
                let sh = Arc::clone(shared);
                HStack::new(cx, move |cx| {
                    for (n, label) in variants.iter().enumerate() {
                        let sh = Arc::clone(&sh);
                        toggle_row(cx, label, sig.map(move |b: &bool| *b as usize == n), move |_cx| {
                            let on = n == 1;
                            sig.set(on);
                            sh.set(i, if on { 1.0 } else { 0.0 });
                        });
                    }
                })
                .height(Auto)
                .horizontal_gap(Pixels(6.0));
            }
            // Plain bool: a single box labelled with the control's short name.
            _ => {
                let sh = Arc::clone(shared);
                toggle_row(cx, short, sig, move |_cx| {
                    let on = !sig.get();
                    sig.set(on);
                    sh.set(i, if on { 1.0 } else { 0.0 });
                });
            }
        },
        Ctl::Buttons(i, sig) | Ctl::Select(i, sig) => {
            let variants = match desc_for_clap_id(i).unwrap().kind {
                ParamKind::Enum { variants } => variants,
                _ => &[],
            };
            let sh = Arc::clone(shared);
            HStack::new(cx, move |cx| {
                for (n, label) in variants.iter().enumerate() {
                    let sh = Arc::clone(&sh);
                    toggle_row(cx, label, sig.map(move |s: &Option<usize>| *s == Some(n)), move |_cx| {
                        sig.set(Some(n));
                        sh.set(i, n as f32);
                    });
                }
            })
            .height(Auto)
            .horizontal_gap(Pixels(6.0));
        }
        // Faders/rotaries never go to the strip.
        Ctl::Fader(..) | Ctl::Rotary(..) => {}
    }
}

/// A vertical exclusive box-list for an enum (the `Buttons`/`Select` controls):
/// one [`toggle_row`] per variant, the box lit on the selected one. The single
/// toggle style used everywhere — source selectors, oversample, cross-mod,
/// assign mode, key modes.
fn enum_list_body(
    cx: &mut Context,
    i: usize,
    sig: SyncSignal<Option<usize>>,
    shared: &Arc<SharedParams>,
) {
    let variants = match desc_for_clap_id(i).unwrap().kind {
        ParamKind::Enum { variants } => variants,
        _ => &[],
    };
    // Display order: natural enum order, except the assign modes read Poly, Twin,
    // Unison, Solo (Twin sits by Poly as the other "thin" mode) — a view reorder
    // only; the stored value is still each variant's own index.
    let order: Vec<usize> = if matches!(param_ref(i), Some(ParamRef::Patch(_, PatchParam::AssignMode)))
    {
        ASSIGN_DISPLAY_ORDER.to_vec()
    } else {
        (0..variants.len()).collect()
    };
    let sh = Arc::clone(shared);
    VStack::new(cx, move |cx| {
        for n in order {
            let label = variants[n];
            let sh = Arc::clone(&sh);
            toggle_row(
                cx,
                label,
                sig.map(move |s: &Option<usize>| *s == Some(n)),
                move |_cx| {
                    sig.set(Some(n));
                    sh.set(i, n as f32);
                    // Assign-mode → Twin narrows the detune range; clamp the value.
                    clamp_detune_on_twin(i, n, &sh);
                },
            );
        }
    })
    .class("tg-list")
    // Content-width so the box-list sits as a tight group (centred by its parent)
    // rather than stretching to fill — keeps selectors snug beside their sliders.
    .width(Auto)
    .height(Auto);
}

fn control_view(cx: &mut Context, ctl: Ctl, shared: &Arc<SharedParams>, short: &'static str) {
    VStack::new(cx, |cx| {
        Label::new(cx, up(short))
            .class("ctl-label")
            .height(Pixels(11.0));
        match ctl {
            Ctl::Fader(i, sig) => fader_body(cx, i, sig, shared, false),
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
                // get small text labels at the same positions.
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
                    // Arc radius for the variant glyphs/labels. Kept small enough
                    // that even the side glyphs sit inside the DIAL box.
                    const R: f32 = 20.0;
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
                            Label::new(cx, up(label))
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
                        .cursor(CursorIcon::Hand)
                        .on_change(move |_cx, v| {
                            // Snap to the nearest variant.
                            let idx = snap(v);
                            sig.set(if cnt > 1 { idx / (cnt - 1) as f32 } else { 0.0 });
                            sh_set.set(i, idx);
                        })
                        .on_hover(move |cx| {
                            posy.set(cursor_top(cx));
                            hover.set(true);
                            show.set(true);
                        })
                        .on_hover_out(move |_cx| {
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
                match desc_for_clap_id(i).unwrap().kind {
                    // A named two-state enum (Sharp/Smooth, Linear/Exponential):
                    // an exclusive two-row list so the state name stays visible.
                    ParamKind::Enum { variants } => {
                        let sh = Arc::clone(shared);
                        VStack::new(cx, move |cx| {
                            for (n, label) in variants.iter().enumerate() {
                                let sh = Arc::clone(&sh);
                                toggle_row(
                                    cx,
                                    label,
                                    sig.map(move |b: &bool| *b as usize == n),
                                    move |_cx| {
                                        let on = n == 1;
                                        sig.set(on);
                                        sh.set(i, if on { 1.0 } else { 0.0 });
                                    },
                                );
                            }
                        })
                        .class("tg-list")
                        .height(Auto);
                    }
                    // Plain on/off bool: a single indicator box, lit when on.
                    _ => {
                        let sh = Arc::clone(shared);
                        toggle_row(cx, "", sig, move |_cx| {
                            let on = !sig.get();
                            sig.set(on);
                            sh.set(i, if on { 1.0 } else { 0.0 });
                        });
                    }
                }
            }
            // All enum pickers — source selectors, oversample, cross-mod,
            // assign — render as the same vertical box-list.
            Ctl::Buttons(i, sig) | Ctl::Select(i, sig) => enum_list_body(cx, i, sig, shared),
        }
    })
    .height(Pixels(COL_H))
    .vertical_gap(Pixels(10.0))
    .alignment(Alignment::TopCenter);
}

/// One modulation-route column (ADR 0004 §4): the column header, then the
/// source-selector box-list **beside** the depth fader (the selector is absent for
/// a fixed source like velocity / pitch-wheel, leaving just the fader). Pairs the
/// route's "where from" and "how much" so the mod panels read as routes rather
/// than a flat cell row.
fn mod_route_view(
    cx: &mut Context,
    header: &'static str,
    src: Option<usize>,
    depth: usize,
    layer: Layer,
    controls: &[Ctl],
    shared: &Arc<SharedParams>,
) {
    let find = |id: usize| {
        controls
            .iter()
            .copied()
            .find(|c| c.idx() == resolve(id, layer))
            .unwrap()
    };
    VStack::new(cx, |cx| {
        Label::new(cx, up(header)).class("ctl-label").height(Pixels(11.0));
        HStack::new(cx, |cx| {
            // Source selector then slider, kept as one tight content-width group
            // centred under the header label (not stretched apart to the column
            // edges).
            if let Some(s) = src {
                match find(s) {
                    Ctl::Buttons(i, sig) | Ctl::Select(i, sig) => {
                        enum_list_body(cx, i, sig, shared)
                    }
                    _ => {}
                }
            }
            if let Ctl::Fader(i, sig) = find(depth) {
                fader_body(cx, i, sig, shared, false);
            }
        })
        .width(Auto)
        .height(Auto)
        .horizontal_gap(Pixels(6.0))
        .alignment(Alignment::TopCenter);
    })
    .height(Pixels(COL_H))
    .vertical_gap(Pixels(6.0))
    .alignment(Alignment::TopCenter);
}

/// The LFO 1 panel's main row: Shape, Rate, Delay, Fade. The Sync and Free
/// toggles drop to the bottom strip (see `in_bottom_strip`), so they're not
/// columns here.
fn lfo1_cells(cx: &mut Context, layer: Layer, controls: &[Ctl], shared: &Arc<SharedParams>) {
    use PatchParam::{Lfo1DelayTime, Lfo1Fade, LfoRate, LfoShape};
    let find = |p: PatchParam| {
        controls
            .iter()
            .copied()
            .find(|c| c.idx() == patch_clap_id(layer, p))
            .unwrap()
    };
    control_view(cx, find(LfoShape), shared, "Shape");
    control_view(cx, find(LfoRate), shared, "Rate");
    control_view(cx, find(Lfo1DelayTime), shared, "Delay");
    control_view(cx, find(Lfo1Fade), shared, "Fade");
}

/// The Cross Mod panel (ADR 0004 §3 + the wide osc-2 pitch route): two
/// selector/fader pairs — the cross-mod **Type** {Off/Sync/PM} with its **Amt**
/// fader, and the osc-2 pitch **Src** {Off/Env1/Env2} with its **Mod** fader.
/// Unlike the route columns, the selector sits *beside* its fader; the fader
/// dims and goes non-interactive while its selector is Off (it drives nothing).
fn cross_mod_panel(cx: &mut Context, layer: Layer, controls: &[Ctl], shared: &Arc<SharedParams>) {
    use PatchParam::*;
    xmod_pair(cx, "Type", CrossModType, "Amt", CrossModAmount, layer, controls, shared);
    xmod_pair(cx, "Src", Osc2PitchEnvSrc, "Mod", Osc2PitchEnvDepth, layer, controls, shared);
}

/// One Cross Mod selector/fader pair: the selector box-list on the left, the
/// depth fader on the right, each under its own label. The fader column dims +
/// disables while the selector reads its first variant (`Off`).
#[allow(clippy::too_many_arguments)]
fn xmod_pair(
    cx: &mut Context,
    sel_label: &'static str,
    sel: PatchParam,
    depth_label: &'static str,
    depth: PatchParam,
    layer: Layer,
    controls: &[Ctl],
    shared: &Arc<SharedParams>,
) {
    let find = |p: PatchParam| {
        controls
            .iter()
            .copied()
            .find(|c| c.idx() == patch_clap_id(layer, p))
            .unwrap()
    };
    let sel_ctl = find(sel);
    let depth_ctl = find(depth);
    // The selector's signal drives whether the depth fader is live. `Off` (variant
    // 0) dims and disables it. A `Binding` rebuilds the fader column on each
    // selector change, so the disable/enable + dim reliably track the selection —
    // a `disabled(memo)` modifier alone doesn't re-fire here.
    let sel_sig = match sel_ctl {
        Ctl::Buttons(_, sig) | Ctl::Select(_, sig) => Some(sig),
        _ => None,
    };
    HStack::new(cx, |cx| {
        VStack::new(cx, |cx| {
            Label::new(cx, up(sel_label)).class("ctl-label").height(Pixels(11.0));
            if let Ctl::Buttons(i, sig) | Ctl::Select(i, sig) = sel_ctl {
                enum_list_body(cx, i, sig, shared);
            }
        })
        .height(Auto)
        .vertical_gap(Pixels(6.0))
        .alignment(Alignment::TopCenter);

        if let (Some(sel_sig), Ctl::Fader(fi, fsig)) = (sel_sig, depth_ctl) {
            let sh = Arc::clone(shared);
            Binding::new(cx, sel_sig, move |cx| {
                // Cross-mod Amt only drives FM (PM depth); it's meaningless for
                // Off/Sync, so enable it solely on FM (CrossModType::Pm). Every
                // other route fader just greys out on its selector's Off.
                let off = if matches!(sel, PatchParam::CrossModType) {
                    sel_sig.get() != Some(CrossModType::Pm as usize)
                } else {
                    sel_sig.get() == Some(0)
                };
                let col = VStack::new(cx, |cx| {
                    Label::new(cx, up(depth_label)).class("ctl-label").height(Pixels(11.0));
                    fader_body(cx, fi, fsig, &sh, off);
                })
                .height(Auto)
                .vertical_gap(Pixels(6.0))
                .alignment(Alignment::TopCenter);
                if off {
                    col.class("dimmed");
                }
            });
        }
    })
    .height(Pixels(COL_H))
    .horizontal_gap(Pixels(2.0))
    .alignment(Alignment::TopCenter);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vxn_engine::Taper;

    #[test]
    fn switching_to_twin_clamps_detune() {
        let s = SharedParams::new();
        let dt = patch_clap_id(Layer::Upper, PatchParam::UnisonDetune);
        let mode = patch_clap_id(Layer::Upper, PatchParam::AssignMode);

        // Wide (Unison-range) detune, then switch to Twin: clamps to the ceiling.
        s.set(dt, 50.0);
        clamp_detune_on_twin(mode, AssignMode::Twin as usize, &s);
        assert_eq!(s.get(dt), TWIN_DETUNE_CT);

        // A value already within range is left alone.
        s.set(dt, 8.0);
        clamp_detune_on_twin(mode, AssignMode::Twin as usize, &s);
        assert_eq!(s.get(dt), 8.0);

        // Switching to a non-Twin mode never touches detune.
        s.set(dt, 50.0);
        clamp_detune_on_twin(mode, AssignMode::Unison as usize, &s);
        assert_eq!(s.get(dt), 50.0);

        // Only the assign-mode picker triggers it (not some other enum at index 3).
        let other = patch_clap_id(Layer::Upper, PatchParam::CrossModType);
        s.set(dt, 50.0);
        clamp_detune_on_twin(other, AssignMode::Twin as usize, &s);
        assert_eq!(s.get(dt), 50.0);
    }

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
    fn mod_routes_cover_their_panel_entries() {
        // The route tables drive the mod-panel layout but the ROWS entries
        // still drive coverage; guard against the two drifting apart — every route
        // id (source + depth) must appear in the panel's entries and vice-versa.
        for (title, routes) in [("Pitch Mod", PITCH_MOD_ROUTES), ("PWM Mod", PWM_MOD_ROUTES)] {
            let entries: &[Entry] = ROWS
                .iter()
                .flat_map(|row| row.iter())
                .find(|(t, _)| *t == title)
                .unwrap()
                .1;
            let mut entry_ids: Vec<usize> = entries.iter().map(|(id, _)| *id).collect();
            let mut route_ids: Vec<usize> = routes
                .iter()
                .flat_map(|(_, src, depth)| src.iter().copied().chain([*depth]))
                .collect();
            entry_ids.sort_unstable();
            route_ids.sort_unstable();
            assert_eq!(entry_ids, route_ids, "{title} routes drifted from entries");
        }
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
    fn env_depth_fader_is_bipolar_full_range() {
        // Env/vel depths span the descriptor's full bipolar range: centre zero,
        // ends ±max (inverting an env is musically meaningful).
        let id = patch_clap_id(Layer::Upper, PatchParam::CutoffEnvDepth);
        let d = desc_for_clap_id(id).unwrap();
        assert!((fader_from_ui(id, 0.0) - d.min).abs() < 1e-3);
        assert!((fader_from_ui(id, 0.5)).abs() < 1e-3); // centre = 0
        assert!((fader_from_ui(id, 1.0) - d.max).abs() < 1e-3);
        for n in [0.1, 0.5, 0.9] {
            assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
        }
    }

    #[test]
    fn lfo_depth_fader_is_unipolar() {
        // LFO depths map 0 → max (bottom is no modulation, not −max).
        for p in [
            PatchParam::PwmLfoDepth,
            PatchParam::CutoffLfo1Depth,
            PatchParam::CutoffLfo2Depth,
        ] {
            let id = patch_clap_id(Layer::Upper, p);
            let d = desc_for_clap_id(id).unwrap();
            assert!(fader_from_ui(id, 0.0).abs() < 1e-4, "bottom should be 0");
            assert!((fader_from_ui(id, 1.0) - d.max).abs() < 1e-3);
            for n in [0.1, 0.5, 0.9] {
                assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn cutoff_and_rate_tapers_centre_correctly() {
        // Filter cutoffs read 1 kHz at the midpoint; LFO rates read 5 Hz.
        for p in [PatchParam::Cutoff, PatchParam::HpfCutoff] {
            let id = patch_clap_id(Layer::Upper, p);
            assert!((fader_from_ui(id, 0.5) - 1000.0).abs() < 1.0, "{p:?} mid");
        }
        let lfo1 = patch_clap_id(Layer::Upper, PatchParam::LfoRate);
        assert!((fader_from_ui(lfo1, 0.5) - 5.0).abs() < 0.01);
        let lfo2 = global_clap_id(GlobalParam::Lfo2Rate);
        assert!((fader_from_ui(lfo2, 0.5) - 5.0).abs() < 0.01);
    }

    #[test]
    fn automation_value_resolves_to_position_and_back() {
        // The idle resync maps a host/automation value to a slider position via
        // `fader_to_ui`; feeding that position back must recover the value (clamped
        // to range). Holds for the non-linear (exp-tapered) faders too, since the
        // mapping is the analytic inverse of the taper — not just for positions the
        // fader itself produced. Sample arbitrary in-range automation values.
        let cases: &[(usize, &[f32])] = &[
            (
                patch_clap_id(Layer::Upper, PatchParam::Cutoff),
                &[20.0, 200.0, 1000.0, 5000.0, 18000.0],
            ),
            (
                patch_clap_id(Layer::Upper, PatchParam::HpfCutoff),
                &[20.0, 440.0, 3000.0],
            ),
            (
                patch_clap_id(Layer::Upper, PatchParam::LfoRate),
                &[0.01, 1.0, 5.0, 23.5, 40.0],
            ),
            (
                global_clap_id(GlobalParam::Lfo2Rate),
                &[0.5, 5.0, 40.0],
            ),
            (
                patch_clap_id(Layer::Upper, PatchParam::PortamentoTime),
                &[0.0, 0.05, 0.1, 0.37, 0.5],
            ),
            (
                patch_clap_id(Layer::Upper, PatchParam::CutoffLfo1Depth),
                &[0.0, 12.0, 48.0, 96.0],
            ),
        ];
        for (id, values) in cases {
            let d = desc_for_clap_id(*id).unwrap();
            for &v in *values {
                let pos = fader_to_ui(*id, v);
                assert!((0.0..=1.0).contains(&pos), "pos {pos} out of range for {v}");
                let back = fader_from_ui(*id, pos);
                let want = v.clamp(d.min, d.max);
                let tol = (want.abs() * 1e-3).max(1e-3);
                assert!(
                    (back - want).abs() <= tol,
                    "value {v} → pos {pos} → {back}, expected {want}"
                );
            }
        }
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
            assert!(matches!(desc_for_clap_id(id).unwrap().taper(), Taper::Exp { .. }));
            assert!(fader_from_ui(id, 0.0).abs() < 1e-4); // ~0 s
            assert!((fader_from_ui(id, 0.5) - 1.0).abs() < 1e-3); // midpoint = 1 s
            assert!((fader_from_ui(id, 1.0) - 10.0).abs() < 1e-3); // top = 10 s
            for n in [0.2, 0.5, 0.8, 1.0] {
                assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
            }
        }
        // Sustain is a level, not a time — stays linear.
        let sus = patch_clap_id(Layer::Upper, PatchParam::Env1Sustain);
        assert_eq!(desc_for_clap_id(sus).unwrap().taper(), Taper::Linear);
    }

    #[test]
    fn pitch_lfo_depth_fader_tapers_to_subtle_vibrato() {
        // 0..12 st, exp-tapered so the lower half of the travel is 0..1 st.
        let id = patch_clap_id(Layer::Upper, PatchParam::PitchLfoDepth);
        assert!(matches!(desc_for_clap_id(id).unwrap().taper(), Taper::Exp { .. }));
        assert!(fader_from_ui(id, 0.0).abs() < 1e-4); // ~0 st
        assert!((fader_from_ui(id, 0.5) - 1.0).abs() < 1e-3); // midpoint = 1 st
        assert!((fader_from_ui(id, 1.0) - 12.0).abs() < 1e-3); // top = 12 st
        for n in [0.2, 0.5, 0.8, 1.0] {
            assert!((fader_to_ui(id, fader_from_ui(id, n)) - n).abs() < 1e-4);
        }
        // The Env→pitch depth stays bipolar/linear — only the LFO depth tapers.
        let env = patch_clap_id(Layer::Upper, PatchParam::PitchEnvDepth);
        assert_eq!(desc_for_clap_id(env).unwrap().taper(), Taper::Linear);
    }

    /// Expand the faceplate `ROWS` into the set of CLAP ids each control binds:
    /// a per-patch entry (baked Upper) is built once per layer, so it covers both
    /// layer ids; a global entry covers itself.
    fn covered_ids() -> Vec<usize> {
        let mut ids = Vec::new();
        for row in ROWS {
            for (_title, entries) in *row {
                for (id, _) in *entries {
                    match param_ref(*id) {
                        Some(ParamRef::Patch(_, p)) => {
                            for layer in Layer::ALL {
                                ids.push(patch_clap_id(layer, p));
                            }
                        }
                        _ => ids.push(*id),
                    }
                }
            }
        }
        ids
    }

    #[test]
    fn every_automatable_param_has_exactly_one_control() {
        // 0023 acceptance: every automatable param has exactly one faceplate
        // control, and there are no orphaned (unbound) or duplicated controls.
        // KeyMode / split point are non-automatable shared state (their own panel)
        // and intentionally absent from the param table.
        let covered = covered_ids();
        for id in 0..TOTAL_PARAMS {
            let n = covered.iter().filter(|c| **c == id).count();
            let desc = desc_for_clap_id(id).unwrap();
            assert_eq!(
                n, 1,
                "param {} ({}) has {} controls, expected exactly 1",
                id, desc.name, n
            );
        }
        // No entry binds an id outside the table.
        for id in &covered {
            assert!(*id < TOTAL_PARAMS, "control bound to out-of-range id {id}");
        }
    }

    #[test]
    fn note_names_are_correct() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4"); // A440
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }
}
