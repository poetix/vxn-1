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
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vizia::ParentWindow;
use vizia::context::TreeProps;
use vizia::prelude::*;
use vizia::vg;
use vxn_app::{
    AssignMode, ControllerHandle, CorpusHandle, CrossModType, DEFAULT_SPLIT_POINT, GlobalParam,
    KeyMode, Layer, ParamDesc, ParamId, ParamKind, ParamModel, ParamRef, PatchParam, PresetCorpus,
    PresetMeta, PresetSource, TOTAL_PARAMS, Tick, UNCATEGORIZED, UiEvent, UserPresetEntry,
    ViewEvent, desc_for_clap_id, global_clap_id, param_ref, patch_clap_id,
};
#[cfg(test)]
use vxn_app::UserFolderEntry;

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

/// The editor's read/write surface to the controller (ADR 0007). Reads go to
/// the [`ParamModel`] (a cheap atomic read on the engine side); writes post a
/// [`UiEvent`] for the controller to apply on its next tick. The audio thread
/// never observes either side — it reads the model's atomics directly.
///
/// Cloning a context clones an `Arc` and a sender (the cost of two
/// reference-count bumps); fine to clone into every move-closure that takes a
/// fader callback.
#[derive(Clone)]
pub struct EditorCtx {
    model: Arc<dyn ParamModel>,
    ctrl: ControllerHandle,
}

impl EditorCtx {
    pub fn new(model: Arc<dyn ParamModel>, ctrl: ControllerHandle) -> Self {
        Self { model, ctrl }
    }

    // ── Reads (proxied to the model) ────────────────────────────────────────

    #[inline]
    fn pid(idx: usize) -> ParamId {
        ParamId::new(idx)
    }
    pub fn get(&self, idx: usize) -> f32 {
        self.model.get(Self::pid(idx))
    }
    pub fn get_normalized(&self, idx: usize) -> f32 {
        self.model.get_normalized(Self::pid(idx))
    }
    pub fn gesture(&self, idx: usize) -> bool {
        self.model.gesture(Self::pid(idx))
    }
    pub fn descriptor(&self, idx: usize) -> Option<&'static ParamDesc> {
        self.model.descriptor(Self::pid(idx))
    }
    pub fn key_mode(&self) -> KeyMode {
        self.model.key_mode()
    }
    pub fn split_point(&self) -> u8 {
        self.model.split_point()
    }

    // ── Writes (posted as UiEvents) ─────────────────────────────────────────

    /// Post a plain-value write. The controller writes the model on next tick
    /// and echoes a `ParamChanged` view event.
    pub fn set(&self, idx: usize, plain: f32) {
        let _ = self.ctrl.post(UiEvent::SetParam {
            id: Self::pid(idx),
            plain,
        });
    }
    pub fn set_normalized(&self, idx: usize, norm: f32) {
        let _ = self.ctrl.post(UiEvent::SetParamNorm {
            id: Self::pid(idx),
            norm,
        });
    }
    pub fn set_gesture(&self, idx: usize, on: bool) {
        let id = Self::pid(idx);
        let _ = self.ctrl.post(if on {
            UiEvent::BeginGesture { id }
        } else {
            UiEvent::EndGesture { id }
        });
    }
    pub fn set_key_mode_seeded(&self, mode: KeyMode) {
        let _ = self.ctrl.post(UiEvent::SetKeyMode { mode });
    }
    pub fn set_split_point(&self, note: u8) {
        let _ = self.ctrl.post(UiEvent::SetSplitPoint { note });
    }
    pub fn reset_patch_to_defaults(&self, layer: Layer) {
        let _ = self.ctrl.post(UiEvent::ResetLayer { layer });
    }
    pub fn post(&self, ev: UiEvent) {
        let _ = self.ctrl.post(ev);
    }
}

pub const EDITOR_WIDTH: u32 = 1024;
/// Four panel rows: (1) LFOs + oscillators + mixer, (2) envelopes + filter +
/// filter mod, (3) the per-osc mod routes + performance wheels, (4) voice +
/// master + keys + the two effects — plus the banner and the preset bar above
/// them (0027).
pub const EDITOR_HEIGHT: u32 = 772;

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
        DelayPingPong, DelaySync, DelayTime, Lfo2Rate, Lfo2Shape, Lfo2Sync, LimiterOn, MasterTune,
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
                // osc1/osc2/ring/noise levels (ADR 0004 §6 / "Panel layout"); the
                // noise colour (White/Pink) picker sits in the bottom strip.
                "Mixer",
                &[
                    (u(Osc1Level), "Osc1"),
                    (u(Osc2Level), "Osc2"),
                    (u(RingLevel), "Ring"),
                    (u(NoiseLevel), "Noise"),
                    (u(NoiseColor), "Col"),
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
                // VCA modulation: amp tremolo (LFO source + depth) and the
                // env-bypass gate switch (in the bottom strip).
                "VCA",
                &[
                    (u(AmpLfoSrc), "LFO"),
                    (u(AmpLfoDepth), "Depth"),
                    (u(AmpEnvBypass), "Gate"),
                ],
            ),
            (
                "Filter",
                &[
                    (u(HpfCutoff), "HPF"),
                    (u(Cutoff), "Cutoff"),
                    (u(Resonance), "Reso"),
                    (u(Drive), "Drive"),
                    (u(FilterMode), "Mode"),
                    // Slope (12/24 dB) + key-track ride the bottom strip together.
                    (u(FilterSlope), "Slope"),
                    (u(FilterKeyTrack), "KeyTrk"),
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
                    // Solo legato. Drawn inside the assign cell (beside the Solo
                    // row), not as its own column — `panel_view` skips it in the
                    // normal cell loop. Listed so it stays a tracked, automatable
                    // control (0023 acceptance).
                    (u(Legato), "Legato"),
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
                    (g(LimiterOn), "Limit"),
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
.wave-glyph { color: #888888; }
.wave-glyph.active { color: #a7cfe2; }
.wave-txt { font-size: 8; color: #888888; }
.wave-txt.active { color: #a7cfe2; }
.dimmed { opacity: 0.35; }
/* Preset bar (0027): a slim strip under the banner. */
.preset-bar { background-color: #1c1c1c; border-width: 1px; border-color: #0e0e0e; corner-radius: 4px; padding-left: 8px; padding-right: 8px; }
.preset-name { font-size: 11; color: #a7cfe2; }
.preset-status { font-size: 9; color: #9a9a9a; }
.pbar-btn { height: 18px; background-color: #333333; border-width: 1px; border-color: #555555; corner-radius: 2px; padding-left: 6px; padding-right: 6px; }
.pbar-btn:hover { background-color: #3a3a3a; border-color: #c4c4c4; }
.pbar-btn .tg-lbl { color: #cfcfcf; font-size: 10; }
.pbar-btn:hover .tg-lbl { color: #ececec; }
.preset-field { background-color: #0e0e0e; border-width: 1px; border-color: #555555; corner-radius: 2px; color: #f0f0f0; font-size: 10; padding-left: 4px; padding-right: 4px; height: 18px; }
.preset-field:focus-visible { border-color: #d9701b; }
/* The vizia default theme makes `caret-color` track `--foreground`, which is the
 * light-mode dark glyph colour by default (we never opt into `.dark`); against
 * the dark `.preset-field` background the caret renders invisible. Force a
 * light caret here so the cursor shows while editing. */
.preset-field:focus.caret { caret-color: #f0f0f0; }
/* Browser panel (0030): two-pane floating preset browser. */
.browser-panel { background-color: #161616; border-width: 1px; border-color: #d9701b; corner-radius: 3px; padding: 6px; }
.browser-search { height: 22px; }
.browser-pane { background-color: #0e0e0e; border-width: 1px; border-color: #2a2a2a; corner-radius: 2px; padding: 2px; }
.browser-section { font-size: 9; color: #d9701b; letter-spacing: 1px; padding-left: 4px; padding-top: 6px; padding-bottom: 6px; }
.browser-row { height: 18px; background-color: transparent; border-width: 0px; padding-left: 6px; padding-right: 4px; corner-radius: 2px; }
.browser-row:hover { background-color: #2a2a2a; }
.browser-row .tg-lbl { color: #d6d6d6; font-size: 10; }
.browser-row:hover .tg-lbl { color: #ffffff; }
.browser-row.selected { background-color: #3a3a3a; }
.browser-row.selected .tg-lbl { color: #ffffff; }
.browser-saveform { padding-top: 4px; }
.browser-saveform-label { font-size: 9; color: #9a9a9a; }
.browser-empty { font-size: 10; color: #6f6f6f; padding-left: 6px; padding-top: 4px; }
/* Context menu (0031): floating list of Rename/Delete/Move to. */
.context-menu { background-color: #1a1a1a; border-width: 1px; border-color: #d9701b; corner-radius: 3px; padding: 2px; }
.context-menu-item { height: 20px; background-color: transparent; border-width: 0px; padding-left: 8px; padding-right: 8px; corner-radius: 2px; }
.context-menu-item:hover { background-color: #d9701b; }
.context-menu-item .tg-lbl { color: #d6d6d6; font-size: 10; }
.context-menu-item:hover .tg-lbl { color: #ffffff; }
.context-menu-item.confirm { background-color: #6a1f1f; }
.context-menu-item.confirm .tg-lbl { color: #ffd0d0; }
.context-menu-item.confirm:hover { background-color: #b03333; }
.context-menu-item.confirm:hover .tg-lbl { color: #ffffff; }
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
fn detune_top(idx: usize, shared: &EditorCtx) -> Option<f32> {
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
fn clamp_detune_on_twin(idx: usize, variant: usize, shared: &EditorCtx) {
    if let Some(ParamRef::Patch(layer, PatchParam::AssignMode)) = param_ref(idx) {
        if variant == AssignMode::Twin as usize {
            let dt = patch_clap_id(layer, PatchParam::UnisonDetune);
            shared.set(dt, shared.get(dt).min(TWIN_DETUNE_CT));
        }
    }
}

/// [`fader_to_ui`] with the live `UnisonDetune` mode scaling applied; identical to
/// the plain mapping for every other fader.
fn fader_to_ui_dyn(idx: usize, value: f32, shared: &EditorCtx) -> f32 {
    match detune_top(idx, shared) {
        Some(top) if top > 0.0 => (value / top).clamp(0.0, 1.0),
        _ => fader_to_ui(idx, value),
    }
}

/// [`fader_from_ui`] with the live `UnisonDetune` mode scaling applied.
fn fader_from_ui_dyn(idx: usize, n: f32, shared: &EditorCtx) -> f32 {
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

fn make_ctl(i: usize, shared: &EditorCtx) -> Ctl {
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
        _ => Ctl::Fader(
            i,
            SyncSignal::new(fader_to_ui_dyn(i, shared.get(i), shared)),
        ),
    }
}

/// Poll message emitted from `on_idle`: tick the controller, then drain its
/// view-event queue into the editor's signals.
struct PollAutomation;

/// Bridges `on_idle` polling to the control signals so DAW automation playback
/// and preset loads repaint the controls. UI-side edits flow the other way
/// through [`EditorCtx`], which posts `UiEvent`s for the controller to apply
/// on its next tick.
struct UiModel {
    controls: Vec<Ctl>,
    /// Read-only handle for the editor (descriptor lookup + the `_dyn` detune
    /// fader's mode-aware mapping needs the live assign-mode value).
    shared: EditorCtx,
    /// Receiver end of the controller's view-event channel. Wrapped in a Mutex
    /// because the controller's Sender side is owned by `vxn-clap`'s main
    /// thread and we only need exclusive access to drain on idle.
    view_rx: Arc<Mutex<Receiver<ViewEvent>>>,
    /// Drain the controller's inbound queues on each tick so UI-posted intents
    /// reach the model promptly (`vxn-clap`'s host-driven `flush` is the only
    /// other tick site and isn't called continuously).
    tick: Tick,
    /// Live snapshot of the user preset corpus; reread on `PresetCorpusChanged`.
    corpus: CorpusHandle,
    /// Mirrors of the non-automatable key-mode state. Set on `KeyModeChanged`.
    key_mode: SyncSignal<usize>,
    split: SyncSignal<f32>,
    /// Browser signals — repopulated on `PresetCorpusChanged`. Preset bar
    /// receives `name` / `status` updates from `PresetLoaded` / `Status`.
    name: SyncSignal<String>,
    status: SyncSignal<String>,
    folders: SyncSignal<Arc<Vec<FolderRow>>>,
    entries: SyncSignal<Arc<Vec<BrowserEntry>>>,
    current: SyncSignal<Option<usize>>,
    selected_folder: SyncSignal<FolderKey>,
    name_field: SyncSignal<String>,
}

impl UiModel {
    /// Dispatch one ParamChanged: look up the bound `Ctl` for this id and
    /// update its signal. Unbound ids (engine-only params with no faceplate
    /// control) are silently dropped.
    fn apply_param_changed(&self, id: ParamId, plain: f32, norm: f32) {
        let idx = id.raw();
        let ctl = self.controls.iter().copied().find(|c| c.idx() == idx);
        let Some(ctl) = ctl else { return };
        match ctl {
            Ctl::Fader(i, sig) => {
                let n = fader_to_ui_dyn(i, plain, &self.shared);
                if (sig.get() - n).abs() > f32::EPSILON {
                    sig.set(n);
                }
            }
            Ctl::Rotary(_, sig) => {
                if (sig.get() - norm).abs() > f32::EPSILON {
                    sig.set(norm);
                }
            }
            Ctl::Switch(_, sig) => {
                let b = plain >= 0.5;
                if sig.get() != b {
                    sig.set(b);
                }
            }
            Ctl::Buttons(_, sig) | Ctl::Select(_, sig) => {
                let s = Some(plain.round() as usize);
                if sig.get() != s {
                    sig.set(s);
                }
            }
        }
    }

    fn apply_corpus_changed(&self, follow: Option<&Path>) {
        let snapshot = match self.corpus.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let (rows, ents) = build_browser_from_corpus(&snapshot);
        // Preserve selected folder if it still exists, else fall back to the
        // first selectable folder.
        let folder_key = self.selected_folder.get();
        let alive = rows.iter().any(|r| match r {
            FolderRow::Folder { key, .. } => key == &folder_key,
            FolderRow::Header(_) => false,
        });
        if !alive {
            self.selected_folder.set(default_folder_key(&rows));
        }
        // Repoint cursor to `follow` path when given, else preserve via the
        // previously-selected user path, else clear.
        let prev_path: Option<PathBuf> = self.current.get().and_then(|i| {
            let es = self.entries.get();
            es.get(i).and_then(|e| match &e.source {
                EntrySource::User(p) => Some(p.clone()),
                EntrySource::Factory(_) => None,
            })
        });
        let target_path: Option<&Path> = follow.or(prev_path.as_deref());
        let new_idx = target_path.and_then(|p| entry_index_for_user_path(&ents, p));
        if let Some(idx) = new_idx {
            if let Some(e) = ents.get(idx) {
                self.name_field.set(e.name.clone());
            }
        }
        self.folders.set(Arc::new(rows));
        self.entries.set(Arc::new(ents));
        self.current.set(new_idx);
    }
}

impl Model for UiModel {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|_msg: &PollAutomation, _meta| {
            // Tick the controller first so any UI events posted since the last
            // tick land in the model and their `ParamChanged` echoes arrive in
            // the view queue we drain immediately below.
            (self.tick)();
            // Drain all pending ViewEvents.
            let mut events: Vec<ViewEvent> = Vec::new();
            if let Ok(rx) = self.view_rx.lock() {
                while let Ok(ev) = rx.try_recv() {
                    events.push(ev);
                }
            }
            for ev in events {
                match ev {
                    ViewEvent::ParamChanged { id, plain, norm, .. } => {
                        self.apply_param_changed(id, plain, norm);
                    }
                    ViewEvent::PresetLoaded { meta, warnings, .. } => {
                        if !meta.name.is_empty() {
                            self.name.set(meta.name.clone());
                            self.name_field.set(meta.name.clone());
                        }
                        let msg = match warnings.first() {
                            None => format!("Loaded {}", meta.name),
                            Some(w) => format!("Loaded {} — {w}", meta.name),
                        };
                        self.status.set(msg);
                    }
                    ViewEvent::PresetCorpusChanged { follow } => {
                        self.apply_corpus_changed(follow.as_deref());
                    }
                    ViewEvent::KeyModeChanged { mode } => {
                        let km = mode as usize;
                        if self.key_mode.get() != km {
                            self.key_mode.set(km);
                        }
                        let sp = self.shared.split_point() as f32;
                        if (self.split.get() - sp).abs() > f32::EPSILON {
                            self.split.set(sp);
                        }
                    }
                    ViewEvent::EditLayerChanged { .. } => {
                        // Vizia owns its own `edit_layer` SyncSignal (set by
                        // the Keys panel's toggle) and ignores the controller
                        // echo. The HTML editor consumes this; we just drop
                        // it here.
                    }
                    ViewEvent::Status { line } => {
                        self.status.set(line);
                    }
                    ViewEvent::OpenTextInput { .. } | ViewEvent::TextInputResult { .. } => {
                        // 0048 (E011) is HTML-faceplate-only — Vizia editor
                        // is on its way out (0054) and never asks the
                        // controller for a text-input popup.
                    }
                }
            }
        });
    }
}

/// Sweeps a stale `delete_confirm` and its open context menu off the screen
/// once the confirm window has elapsed. The first delete click queues a
/// `DeleteConfirm`; this model's `PollAutomation` handler is the only place
/// that *forgets* it if the user never clicks a second time. Without it, the
/// row stays in its "Click to confirm" state and the menu remains open until
/// the user clicks elsewhere.
struct DeleteSweeper {
    delete_confirm: SyncSignal<Option<DeleteConfirm>>,
    context_menu: SyncSignal<Option<ContextMenu>>,
}

impl Model for DeleteSweeper {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|_msg: &PollAutomation, _meta| {
            let Some(dc) = self.delete_confirm.get() else {
                return;
            };
            if dc.at.elapsed() < Duration::from_millis(DELETE_CONFIRM_MS) {
                return;
            }
            self.delete_confirm.set(None);
            self.context_menu.set(None);
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
/// Tag a built Handle with a probe identifier when the `layout-probe` feature
/// is on, drop it otherwise. The handle is taken by value (vizia's `.id` is
/// `fn(mut self, ..) -> Self`), so this is the only way the probe annotations
/// don't litter the call site with unused-variable noise off-feature.
macro_rules! probe_id {
    ($handle:expr, $name:expr) => {{
        #[cfg(feature = "layout-probe")]
        {
            let _ = $handle.id($name);
        }
        #[cfg(not(feature = "layout-probe"))]
        {
            let _ = $handle;
        }
    }};
}

/// Debug-only layout probe (feature `layout-probe`, off in shipped builds).
/// `.id("vxn:…")` identifiers picked up by the layout probe walk, dumped to
/// JSONL on the first idle frame. List built once, in [`probed_ids`], so the
/// probe targets stay grep-able from one place.
#[cfg(feature = "layout-probe")]
pub fn probed_ids() -> Vec<String> {
    let mut v = vec![
        "vxn:Banner".to_string(),
        "vxn:PresetBar".to_string(),
        "vxn:KeysPanel".to_string(),
        "vxn:Row1".to_string(),
        "vxn:Row2".to_string(),
        "vxn:Row3".to_string(),
        "vxn:Row4".to_string(),
    ];
    // Every named panel in ROWS gets `.id("vxn:Panel::{title}")` (or, for
    // layer-dependent panels, the per-layer variants — only the visible one
    // resolves; the hidden one's bounds are still computed).
    for row in ROWS {
        for (title, _) in *row {
            v.push(format!("vxn:Panel::{title}"));
            v.push(format!("vxn:Panel::{title}:Upper"));
            v.push(format!("vxn:Panel::{title}:Lower"));
        }
    }
    v
}

/// Standalone layout probe entry point.
///
/// After ADR 0007 the editor needs a controller + model to build, neither of
/// which lives in `vxn-ui-vizia`. The standalone probe is therefore a no-op stub —
/// run the probe by loading the plugin in a host with the `layout-probe`
/// feature on (the running editor still writes `target/vxn-layout.jsonl`).
#[cfg(feature = "layout-probe")]
pub fn run_layout_probe() {
    eprintln!(
        "run_layout_probe is no longer self-hosting after ADR 0007 — load the \
         plugin in a host with --features layout-probe to drive layout dumps."
    );
}

/// Write the resolved bounds for every entity in [`probed_ids`] to the JSONL
/// file at `VXN_PROBE_OUT` (defaults to `target/vxn-layout.jsonl`). One line
/// per id: `{"name":..., "x":..., "y":..., "w":..., "h":...}` in logical px.
#[cfg(feature = "layout-probe")]
fn dump_layout(cx: &mut EventContext) {
    use std::fs::File;
    use std::io::Write;
    let path = std::env::var("VXN_PROBE_OUT")
        .unwrap_or_else(|_| "target/vxn-layout.jsonl".to_string());
    let Ok(mut f) = File::create(&path) else {
        eprintln!("layout-probe: cannot open {path}");
        return;
    };
    let s = cx.scale_factor().max(1.0);
    for name in probed_ids() {
        let Some(entity) = cx.resolve_entity_identifier(&name) else {
            continue;
        };
        let b = cx.with_current(entity, |c| c.bounds());
        let _ = writeln!(
            f,
            "{{\"name\":\"{}\",\"x\":{:.1},\"y\":{:.1},\"w\":{:.1},\"h\":{:.1}}}",
            name,
            b.x / s,
            b.y / s,
            b.w / s,
            b.h / s
        );
    }
    eprintln!("layout-probe: wrote {path}");
}

/// Open the editor parented to `parent` (on macOS the host `NSView`).
///
/// The editor is now driven entirely through the controller (ADR 0007):
///
/// - `model` is read for descriptor lookup + the mode-aware UnisonDetune fader.
/// - `ctrl` is the post handle for `UiEvent`s; every UI write goes through it.
/// - `view_rx` is the controller → view channel; drained on idle.
/// - `corpus` is the shared preset-corpus snapshot the browser reads.
/// - `tick` is the controller's tick closure (`Controller::tick` wrapped in a
///   Mutex by the clack shell), called on idle so UI-posted intents apply
///   promptly rather than waiting for the host's `flush`.
///
/// `scale_override` pins the HiDPI factor — see the older doc for why
/// `SystemScaleFactor` is unreliable on macOS.
pub fn open_editor(
    parent: *mut c_void,
    model: Arc<dyn ParamModel>,
    ctrl: ControllerHandle,
    view_rx: Arc<Mutex<Receiver<ViewEvent>>>,
    corpus: CorpusHandle,
    tick: Tick,
    scale_override: Option<f64>,
) -> EditorHandle {
    let parent = ParentWindow(parent);
    let shared = EditorCtx::new(model, ctrl);

    // Per-open idle tick counter (interior-mutable so the `Fn` idle closure can
    // bump it). Process-static would leak across reopens and skip the one-time
    // surface rebuild on the second window.
    let idle_tick = std::cell::Cell::new(0u32);

    let build_shared = shared.clone();
    let build_view_rx = view_rx.clone();
    let build_corpus = corpus.clone();
    let build_tick = tick.clone();
    let mut app = Application::new(move |cx| {
        build_editor(
            cx,
            build_shared.clone(),
            build_view_rx.clone(),
            build_corpus.clone(),
            build_tick.clone(),
        )
    })
    .on_idle(move |cx| {
        // Broadcast to every model in the tree, not just root. `cx.emit`
        // propagates `Up` from `cx.current` (root), so only root-attached
        // models would see it — the browser panel's `DeleteSweeper` lives
        // deeper than that.
        cx.emit_custom(
            Event::new(PollAutomation)
                .target(Entity::root())
                .propagate(Propagation::Subtree),
        );
        let n = idle_tick.get();
        idle_tick.set(n.saturating_add(1));
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

fn build_editor(
    cx: &mut Context,
    shared: EditorCtx,
    view_rx: Arc<Mutex<Receiver<ViewEvent>>>,
    corpus: CorpusHandle,
    tick: Tick,
) {
    // Bundle the faceplate font so it renders identically on any host/OS.
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

    // Key-mode UI state. `edit_layer` is pure view state; `key_mode` and `split`
    // mirror non-automatable shared state, kept in sync via `KeyModeChanged`
    // ViewEvents from the controller (state-load and UI-edit both go via it).
    let edit_layer = SyncSignal::new(0usize);
    let key_mode = SyncSignal::new(shared.key_mode() as usize);
    let split = SyncSignal::new(shared.split_point() as f32);

    // Browser signals — initial values seeded from the corpus snapshot the
    // controller already published; subsequent `PresetCorpusChanged` events
    // rebuild from the same snapshot under its mutex.
    let initial_snapshot = match corpus.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let (initial_rows, initial_entries) = build_browser_from_corpus(&initial_snapshot);
    let default_folder = default_folder_key(&initial_rows);
    let folders: SyncSignal<Arc<Vec<FolderRow>>> = SyncSignal::new(Arc::new(initial_rows));
    let entries: SyncSignal<Arc<Vec<BrowserEntry>>> = SyncSignal::new(Arc::new(initial_entries));

    let name = SyncSignal::new(String::from("\u{2014}"));
    let status = SyncSignal::new(String::new());
    let current: SyncSignal<Option<usize>> = SyncSignal::new(None);
    let selected_folder: SyncSignal<FolderKey> = SyncSignal::new(default_folder);
    let name_field = SyncSignal::new(String::new());

    UiModel {
        controls: controls.clone(),
        shared: shared.clone(),
        view_rx: view_rx.clone(),
        tick,
        corpus: corpus.clone(),
        key_mode,
        split,
        name,
        status,
        folders,
        entries,
        current,
        selected_folder,
        name_field,
    }
    .build(cx);

    ScrollView::new(cx, move |cx| {
        VStack::new(cx, |cx| {
            // Branding banner across the top, pushing the panel rows down.
            let banner = Label::new(cx, "VULPUS LABS - VXN-1")
                .class("banner")
                .width(Stretch(1.0))
                .height(Pixels(26.0))
                .alignment(Alignment::Center);
            probe_id!(banner, "vxn:Banner");
            // Preset browser bar (0027). Built against the UiModel's signals so
            // the controller's view-event drain populates everything.
            preset_bar(
                cx,
                &shared,
                folders,
                entries,
                name,
                status,
                current,
                selected_folder,
                name_field,
            );
            for (row_idx, row) in ROWS.iter().enumerate() {
                let _ = row_idx;
                let row_handle = HStack::new(cx, |cx| {
                    for (title, entries) in *row {
                        if *title == "Keys" {
                            keys_panel(cx, &shared, edit_layer, key_mode, split);
                        } else if is_layer_dependent(entries) {
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
                probe_id!(row_handle, format!("vxn:Row{}", row_idx + 1));
            }
        })
        .vertical_gap(Pixels(8.0))
        .padding(Pixels(10.0));
    });
}

/// Where a browser entry's preset is read from.
#[derive(Clone)]
enum EntrySource {
    /// Index into the embedded factory bank (`vxn_engine::factory()`).
    Factory(usize),
    /// Path to a `.toml` in the user preset directory.
    User(PathBuf),
}

/// Identifies one folder in the two-pane browser (ADR 0006 §1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FolderKey {
    /// Factory category (`meta.category`, name-sorted on the left pane).
    Factory(String),
    /// The virtual root group (loose `.toml`s under [`user_preset_dir`]).
    UserRoot,
    /// A real subdirectory under [`user_preset_dir`].
    User(String),
}

impl FolderKey {
    fn is_factory(&self) -> bool {
        matches!(self, FolderKey::Factory(_))
    }
    /// Save target for `save_performance_in`: `None` = user root, `Some(name)`
    /// = subfolder. `None` *return* means factory — not a writable target.
    fn save_target(&self) -> Option<Option<String>> {
        match self {
            FolderKey::UserRoot => Some(None),
            FolderKey::User(n) => Some(Some(n.clone())),
            FolderKey::Factory(_) => None,
        }
    }
}

/// One row in the left pane: either a section header or a selectable folder.
#[derive(Clone)]
enum FolderRow {
    Header(String),
    Folder { key: FolderKey, label: String },
}

/// Which target the inline-rename widget should attach to. New Folder sets it
/// on the freshly-made folder; the context menu's Rename action sets it on the
/// clicked preset or folder.
#[derive(Clone, Debug, PartialEq)]
enum RenameTarget {
    Folder(String),
    Preset(PathBuf),
}

/// Target of an open context menu (ADR 0006 §7). Factory rows do not open one,
/// so this is always a user-side target. Folders only get Rename + Delete;
/// presets get Rename + Delete + Move to ▸.
#[derive(Clone, Debug, PartialEq)]
enum MenuTarget {
    UserFolder(String),
    UserPreset {
        path: PathBuf,
        /// Carried so Move to ▸ can grey out the current folder.
        folder: FolderKey,
    },
}

/// One open context menu. The menu is a child of the right-clicked row;
/// `anchor_x` / `anchor_y` are the cursor's offset within that row (window
/// cursor minus row bounds), which is also what the menu's `Absolute`
/// `top` / `left` resolve against — so the menu lands at the click point.
/// `ignore_clipping(true)` lets it escape the ScrollView's clip path.
#[derive(Clone, Debug, PartialEq)]
struct ContextMenu {
    target: MenuTarget,
    anchor_x: f32,
    anchor_y: f32,
    submenu_open: bool,
}

/// Pending delete: first menu click queues, second click within
/// `DELETE_CONFIRM_MS` commits. Cleared by any other action or by timeout.
#[derive(Clone, Debug, PartialEq)]
struct DeleteConfirm {
    target: MenuTarget,
    at: Instant,
}

const DELETE_CONFIRM_MS: u64 = 3000;

/// Move-to ▸ submenu rows. Pure helper so it can be unit-tested without a UI
/// context. Order follows the left-pane order: `Uncategorised` first, then
/// each user subfolder alpha-sorted. The current folder is suppressed (moving
/// to the current folder is a no-op).
fn move_targets(rows: &[FolderRow], current: &FolderKey) -> Vec<(FolderKey, String)> {
    let mut out: Vec<(FolderKey, String)> = Vec::new();
    for r in rows {
        let FolderRow::Folder { key, label } = r else {
            continue;
        };
        match key {
            FolderKey::UserRoot | FolderKey::User(_) => {
                if key == current {
                    continue;
                }
                out.push((key.clone(), label.clone()));
            }
            FolderKey::Factory(_) => {}
        }
    }
    out
}

/// One row in the browser's combined Factory+User list. Folder is carried so
/// the right-pane filter and the prev/next walker don't need to chase paths.
#[derive(Clone)]
struct BrowserEntry {
    name: String,
    folder: FolderKey,
    source: EntrySource,
}

/// Parsed search-box state: the raw whitespace-trimmed input lowercased, used
/// as a substring match against `meta.name`. (Earlier drafts split out `#tag`
/// tokens — see [[VXN1 preset system]] — but the tag concept was dropped;
/// category is the only browser discriminator now.)
#[derive(Clone, Debug, Default, PartialEq)]
struct SearchQuery {
    text: String,
}

fn parse_search(input: &str) -> SearchQuery {
    SearchQuery {
        text: input.trim().to_lowercase(),
    }
}

impl SearchQuery {
    fn matches(&self, entry: &BrowserEntry) -> bool {
        self.text.is_empty() || entry.name.to_lowercase().contains(&self.text)
    }
}

/// Build the left-pane folder rows and the combined flat preset list together.
///
/// Folder order (ADR 0006 §1, ticket 0030): factory categories alpha-sorted
/// under a `Factory` header, then user folders (`Uncategorised` first, then
/// each subfolder alpha-sorted) under a `User` header. The combined flat list
/// — the prev/next walker — follows the same folder order, name-sorted within
/// each folder. Factory `EntrySource::Factory(i)` indices point back into the
/// unsorted bank.
/// Build the left-pane folder rows and the combined flat preset list from the
/// controller's [`PresetCorpus`] snapshot. Pure function — same display order
/// as the pre-controller version (factory categories alpha-sorted, then user
/// folders with `Uncategorised` first), so the prev/next walker and the
/// folder sort are unaffected by where the data is sourced.
fn build_browser_from_corpus(
    corpus: &PresetCorpus,
) -> (Vec<FolderRow>, Vec<BrowserEntry>) {
    // Factory: sort by (category, name); each `Factory(i)` index keeps
    // pointing into the unsorted bank so `UiEvent::LoadPreset` round-trips.
    let mut indexed: Vec<(usize, &PresetMeta)> = corpus.factory.iter().enumerate().collect();
    indexed.sort_by(|a, b| {
        let ca = a.1.category.as_deref().unwrap_or("");
        let cb = b.1.category.as_deref().unwrap_or("");
        ca.to_lowercase()
            .cmp(&cb.to_lowercase())
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
    });

    let mut factory_cats: Vec<String> = Vec::new();
    let mut entries: Vec<BrowserEntry> = Vec::new();
    for (i, m) in indexed {
        let cat = m.category.clone().unwrap_or_else(|| "Factory".to_string());
        if !factory_cats
            .last()
            .map(|c: &String| c.eq_ignore_ascii_case(&cat))
            .unwrap_or(false)
        {
            factory_cats.push(cat.clone());
        }
        entries.push(BrowserEntry {
            name: m.name.clone(),
            folder: FolderKey::Factory(cat),
            source: EntrySource::Factory(i),
        });
    }

    // User folders: root group first, then subfolders alpha-sorted. The
    // PresetStore impl already produces this order, but we re-sort defensively
    // so a hand-built corpus (tests) lands in the documented shape too.
    let mut root_presets: &[UserPresetEntry] = &[];
    let mut subfolders: Vec<(&str, &[UserPresetEntry])> = Vec::new();
    for uf in &corpus.user {
        match &uf.name {
            None => root_presets = &uf.presets,
            Some(name) => subfolders.push((name.as_str(), &uf.presets)),
        }
    }
    subfolders.sort_by_key(|(n, _)| n.to_lowercase());

    for p in root_presets {
        entries.push(BrowserEntry {
            name: p.meta.name.clone(),
            folder: FolderKey::UserRoot,
            source: EntrySource::User(p.path.clone()),
        });
    }
    for (name, presets) in &subfolders {
        let key = FolderKey::User((*name).to_string());
        for p in *presets {
            entries.push(BrowserEntry {
                name: p.meta.name.clone(),
                folder: key.clone(),
                source: EntrySource::User(p.path.clone()),
            });
        }
    }

    let mut rows: Vec<FolderRow> = Vec::new();
    if !factory_cats.is_empty() {
        rows.push(FolderRow::Header("Factory".to_string()));
        for cat in &factory_cats {
            rows.push(FolderRow::Folder {
                key: FolderKey::Factory(cat.clone()),
                label: cat.clone(),
            });
        }
    }
    rows.push(FolderRow::Header("User".to_string()));
    rows.push(FolderRow::Folder {
        key: FolderKey::UserRoot,
        label: UNCATEGORIZED.to_string(),
    });
    for (name, _) in &subfolders {
        rows.push(FolderRow::Folder {
            key: FolderKey::User((*name).to_string()),
            label: (*name).to_string(),
        });
    }
    (rows, entries)
}

/// First selectable folder key in the row list — used as the default selection
/// when the browser opens cold. Factory categories come first, so this is
/// typically the alpha-first factory category.
fn default_folder_key(rows: &[FolderRow]) -> FolderKey {
    for r in rows {
        if let FolderRow::Folder { key, .. } = r {
            return key.clone();
        }
    }
    FolderKey::UserRoot
}

/// Next index when stepping the combined list by `delta` (wraps). `None` only for
/// an empty list; with no current selection a forward step starts at the first
/// entry and a backward step at the last.
fn step_index(delta: isize, current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(i) => (i as isize + delta).rem_euclid(len as isize) as usize,
        None if delta >= 0 => 0,
        None => len - 1,
    })
}

/// Locate one user preset's index in `entries` by on-disk path.
fn entry_index_for_user_path(entries: &[BrowserEntry], path: &Path) -> Option<usize> {
    entries
        .iter()
        .position(|e| matches!(&e.source, EntrySource::User(p) if p == path))
}

/// Time window for the panel's "second press-down on the same row = load"
/// double-click. Implemented manually rather than via vizia's `on_double_click`
/// because hover dispatch inside the absolute-positioned panel is fragile
/// (`vxn1-vizia-no-click-slop`, ADR 0006 §7).
const DOUBLE_PRESS_MS: u128 = 350;

/// Populate the save-form Name field and the panel's `current` cursor from an
/// entry. Does **not** load — used when single-clicking a row, so the user
/// can edit the name before re-saving.
fn select_entry(
    idx: usize,
    entry: &BrowserEntry,
    current: SyncSignal<Option<usize>>,
    selected_folder: SyncSignal<FolderKey>,
    name_field: SyncSignal<String>,
) {
    current.set(Some(idx));
    selected_folder.set(entry.folder.clone());
    name_field.set(entry.name.clone());
}

/// Post a `LoadPreset` for the entry at `idx` and update the cursor/folder/
/// name-field signals locally. The current preset name and status line are
/// updated by the controller's view-event drain (`PresetLoaded`), not here.
#[allow(clippy::too_many_arguments)]
fn load_entry(
    idx: usize,
    entries: &[BrowserEntry],
    shared: &EditorCtx,
    current: SyncSignal<Option<usize>>,
    selected_folder: SyncSignal<FolderKey>,
    name_field: SyncSignal<String>,
) {
    let Some(entry) = entries.get(idx) else {
        return;
    };
    let source = match &entry.source {
        EntrySource::Factory(i) => PresetSource::Factory { index: *i },
        EntrySource::User(p) => PresetSource::User { path: p.clone() },
    };
    shared.post(UiEvent::LoadPreset { source });
    select_entry(idx, entry, current, selected_folder, name_field);
}

/// The preset browser bar (0030) and its floating two-pane browser panel
/// (folders | presets, search row top, save form bottom — ADR 0006 §§1–§4).
/// The bar carries the current preset name with prev/next steppers (still
/// walking the combined Factory+User list in folder-then-name order) and a
/// Browse toggle that opens the panel. Everything Save-related — name and tag
/// editing, New Folder, Load — lives inside the panel.
///
/// Built against the editor idiom: `SyncSignal` state, `on_press_down` (vizia
/// drops Press on tiny cursor drift, [[vxn1-vizia-no-click-slop]]), and the
/// existing `PollAutomation` idle resync — a one-shot bulk load repaints every
/// control on the next idle tick rather than continuous relayout, so it
/// doesn't stomp input ([[vxn1-vizia-automation-relayout-input-stomp]]).
#[allow(clippy::too_many_arguments)]
fn preset_bar(
    cx: &mut Context,
    shared: &EditorCtx,
    folders: SyncSignal<Arc<Vec<FolderRow>>>,
    entries: SyncSignal<Arc<Vec<BrowserEntry>>>,
    name: SyncSignal<String>,
    status: SyncSignal<String>,
    current: SyncSignal<Option<usize>>,
    selected_folder: SyncSignal<FolderKey>,
    name_field: SyncSignal<String>,
) {
    let shared = shared.clone();
    let browse_open = SyncSignal::new(false);

    // Panel state. `search` narrows the right pane; the name field is the
    // Save-As payload (prefilled on selection so editing then saving
    // overwrites the same file).
    let search = SyncSignal::new(String::new());

    // Manual double-click tracking: a press-down on the same row inside the
    // window loads. Resets on row change.
    let last_press: SyncSignal<Option<(usize, Instant)>> = SyncSignal::new(None);

    // Inline-rename widget reads this signal to know which row to swap for a
    // Textbox. The New Folder button sets it on the freshly created folder;
    // the context menu's Rename action sets it on the clicked row.
    let rename_target: SyncSignal<Option<RenameTarget>> = SyncSignal::new(None);

    // Open context menu (right-clicked user row) + pending delete confirmation.
    let context_menu: SyncSignal<Option<ContextMenu>> = SyncSignal::new(None);
    let delete_confirm: SyncSignal<Option<DeleteConfirm>> = SyncSignal::new(None);

    DeleteSweeper {
        delete_confirm,
        context_menu,
    }
    .build(cx);

    let preset_bar_handle = HStack::new(cx, move |cx| {
        // ── Prev / current name / next ──
        let sh_prev = shared.clone();
        Button::new(cx, |cx| Label::new(cx, "<").class("tg-lbl"))
            .class("pbar-btn")
            .cursor(CursorIcon::Hand)
            .on_press_down(move |_cx| {
                let es = entries.get();
                if let Some(ni) = step_index(-1, current.get(), es.len()) {
                    load_entry(ni, &es, &sh_prev, current, selected_folder, name_field);
                }
            });
        Label::new(cx, name)
            .class("preset-name")
            .width(Pixels(150.0))
            .height(Stretch(1.0))
            .alignment(Alignment::Left);
        let sh_next = shared.clone();
        Button::new(cx, |cx| Label::new(cx, ">").class("tg-lbl"))
            .class("pbar-btn")
            .cursor(CursorIcon::Hand)
            .on_press_down(move |_cx| {
                let es = entries.get();
                if let Some(ni) = step_index(1, current.get(), es.len()) {
                    load_entry(ni, &es, &sh_next, current, selected_folder, name_field);
                }
            });

        // ── Browse toggle + floating panel ──
        let sh_panel = shared.clone();
        VStack::new(cx, move |cx| {
            Button::new(cx, |cx| Label::new(cx, "Browse").class("tg-lbl"))
                .class("pbar-btn")
                .cursor(CursorIcon::Hand)
                .z_index(250)
                .on_press_down(move |_cx| browse_open.set(!browse_open.get()));

            Binding::new(cx, browse_open, move |cx| {
                if !browse_open.get() {
                    return;
                }
                let sh_panel = sh_panel.clone();
                browser_panel(
                    cx,
                    sh_panel,
                    folders,
                    entries,
                    name,
                    status,
                    current,
                    browse_open,
                    selected_folder,
                    search,
                    name_field,
                    last_press,
                    rename_target,
                    context_menu,
                    delete_confirm,
                );
            });
        })
        .width(Auto)
        .height(Stretch(1.0))
        .alignment(Alignment::Center);

        // Status / warning line fills the remaining width on the right.
        Label::new(cx, status)
            .class("preset-status")
            .width(Stretch(1.0))
            .height(Stretch(1.0))
            .alignment(Alignment::Right);
    })
    .class("preset-bar")
    .height(Pixels(30.0))
    .horizontal_gap(Pixels(6.0))
    .alignment(Alignment::Left);
    probe_id!(preset_bar_handle, "vxn:PresetBar");
}

/// Bundle of every browser signal a mutation handler needs (rename / delete /
/// move / save / new folder). Saves passing eight arguments around the inline
/// closures used by the rows and the context menu.
#[derive(Clone, Copy)]
struct BrowserSignals {
    folders: SyncSignal<Arc<Vec<FolderRow>>>,
    entries: SyncSignal<Arc<Vec<BrowserEntry>>>,
    current: SyncSignal<Option<usize>>,
    selected_folder: SyncSignal<FolderKey>,
    name_field: SyncSignal<String>,
    name: SyncSignal<String>,
    /// Reserved for future direct-status writes; current code routes status
    /// through controller `Status` events and `UiModel`'s drain.
    #[allow(dead_code)]
    status: SyncSignal<String>,
    rename_target: SyncSignal<Option<RenameTarget>>,
    context_menu: SyncSignal<Option<ContextMenu>>,
    delete_confirm: SyncSignal<Option<DeleteConfirm>>,
}

/// Post a folder-rename intent. The controller does the IO and emits a
/// `PresetCorpusChanged` view event; the editor's `UiModel` drain re-reads
/// the corpus snapshot and the inline editor closes via `rename_target.set(None)`.
fn commit_folder_rename(ctx: &EditorCtx, old: &str, new: &str, sigs: BrowserSignals) {
    let trimmed = new.trim().to_string();
    // Move the folder selection optimistically so the right pane keeps
    // showing the same presets when the rename succeeds. (If it fails, the
    // next corpus refresh will land it on the first selectable folder.)
    if sigs.selected_folder.get() == FolderKey::User(old.to_string()) {
        sigs.selected_folder.set(FolderKey::User(trimmed.clone()));
    }
    sigs.rename_target.set(None);
    ctx.post(UiEvent::RenameFolder {
        old_name: old.to_string(),
        new_name: trimmed,
    });
}

/// Post a preset-rename intent. Cursor follows the renamed file via the
/// `PresetCorpusChanged { follow }` event the controller emits.
fn commit_preset_rename(ctx: &EditorCtx, path: &Path, new: &str, sigs: BrowserSignals) {
    let trimmed = new.trim().to_string();
    sigs.rename_target.set(None);
    sigs.name.set(trimmed.clone());
    ctx.post(UiEvent::RenamePreset {
        path: path.to_path_buf(),
        new_name: trimmed,
    });
}

/// Post a folder-delete intent (recursive). Selection falls back to the
/// first selectable folder via the corpus drain.
fn delete_folder(ctx: &EditorCtx, name: &str, _sigs: BrowserSignals) {
    ctx.post(UiEvent::DeleteFolder {
        name: name.to_string(),
    });
}

/// Post a preset-delete intent.
fn delete_preset(ctx: &EditorCtx, path: &Path, _sigs: BrowserSignals) {
    ctx.post(UiEvent::DeletePreset {
        path: path.to_path_buf(),
    });
}

/// Post a preset-move intent. The cursor follows the moved file (same
/// filename, new parent) via the controller's `PresetCorpusChanged { follow }`
/// event.
fn move_preset(ctx: &EditorCtx, path: &Path, dest: Option<&str>, sigs: BrowserSignals) {
    // Match the destination folder so the right pane shows the moved entry
    // without the user having to click the destination row.
    sigs.selected_folder.set(match dest {
        None => FolderKey::UserRoot,
        Some(n) => FolderKey::User(n.to_string()),
    });
    ctx.post(UiEvent::MovePreset {
        path: path.to_path_buf(),
        dest_folder: dest.map(str::to_string),
    });
}

/// Dismiss the context menu and clear any pending delete-confirm. Called on
/// any committed action and on outside-click.
fn close_menu(sigs: BrowserSignals) {
    sigs.context_menu.set(None);
    sigs.delete_confirm.set(None);
}

/// One folder row in the left pane. Factory rows are inert label-buttons; user
/// rows additionally open the context menu on right-click and swap in an
/// inline rename textbox when this folder is the `rename_target`.
fn folder_row(
    cx: &mut Context,
    key: FolderKey,
    label: String,
    sigs: BrowserSignals,
    shared: EditorCtx,
) {
    let key_cmp = key.clone();
    let selected = sigs.selected_folder.map(move |s: &FolderKey| *s == key_cmp);
    let rename_self: Option<RenameTarget> = match &key {
        FolderKey::User(n) => Some(RenameTarget::Folder(n.clone())),
        _ => None,
    };
    let renaming_btn = {
        let rs = rename_self.clone();
        sigs.rename_target.map(move |t: &Option<RenameTarget>| {
            rs.as_ref().is_some_and(|r| t.as_ref() == Some(r))
        })
    };
    let not_renaming = renaming_btn.map(|b: &bool| !b);

    let key_for_click = key.clone();
    let key_for_right = key.clone();
    let label_for_btn = label.clone();
    Button::new(cx, move |cx| {
        Label::new(cx, label_for_btn.clone())
            .class("tg-lbl")
            .hoverable(false)
    })
    .class("browser-row")
    .toggle_class("selected", selected)
    .width(Stretch(1.0))
    .cursor(CursorIcon::Hand)
    .display(not_renaming)
    .on_press_down(move |_cx| {
        sigs.selected_folder.set(key_for_click.clone());
        close_menu(sigs);
    })
    .on_mouse_down(move |cx, btn| {
        if btn != MouseButton::Right {
            return;
        }
        let FolderKey::User(n) = &key_for_right else {
            return;
        };
        // The menu Binding's children share a layout parent with this Button
        // (the wrapping `Binding`s are layout-ignored, so children are
        // re-parented up to the nearest non-ignored ancestor). Anchor in that
        // shared parent's coord system, not the button's.
        //
        // Convert physical-pixel offsets to logical — vizia's cursor coords
        // and cache bounds are physical, but `Pixels(N)` is treated as logical
        // and re-multiplied by the dpi factor at layout time. On a Retina
        // display feeding physical anchors straight in lands the menu at 2×
        // the cursor offset.
        let pb = cx.cache.get_bounds(cx.parent());
        let m = cx.mouse();
        sigs.context_menu.set(Some(ContextMenu {
            target: MenuTarget::UserFolder(n.clone()),
            anchor_x: cx.physical_to_logical(m.cursor_x - pb.x),
            anchor_y: cx.physical_to_logical(m.cursor_y - pb.y),
            submenu_open: false,
        }));
        sigs.delete_confirm.set(None);
    });

    // Context menu (anchored just below this row). Rendered only when this
    // row's target is the active menu — `ignore_clipping` lets it escape the
    // left-pane ScrollView's clip.
    if let FolderKey::User(n) = &key {
        let my_target = MenuTarget::UserFolder(n.clone());
        let shared_menu = shared.clone();
        Binding::new(cx, sigs.context_menu, move |cx| {
            let Some(menu) = sigs.context_menu.get() else {
                return;
            };
            if menu.target != my_target {
                return;
            }
            context_menu_view(cx, menu, sigs, shared_menu.clone());
        });
    }

    if let Some(rt) = rename_self {
        let rt_match = rt.clone();
        let renaming = sigs
            .rename_target
            .map(move |t: &Option<RenameTarget>| t.as_ref() == Some(&rt_match));
        let old_name = match &rt {
            RenameTarget::Folder(n) => n.clone(),
            RenameTarget::Preset(_) => String::new(),
        };
        let initial = label;
        Binding::new(cx, renaming, move |cx| {
            if !renaming.get() {
                return;
            }
            let buf = SyncSignal::new(initial.clone());
            let old = old_name.clone();
            let shared_for = shared.clone();
            Textbox::new(cx, buf)
                .class("preset-field")
                .width(Stretch(1.0))
                .focused(true)
                .on_edit(move |_cx, t| buf.set(t))
                .on_submit(move |_cx, t: String, _success| {
                    commit_folder_rename(&shared_for, &old, &t, sigs);
                })
                .on_blur(move |_cx| sigs.rename_target.set(None))
                .on_cancel(move |_cx| sigs.rename_target.set(None));
        });
    }
}

/// One preset row in the right pane. Factory rows skip the right-click menu
/// and the inline rename swap (factory is immutable, ADR 0006 §5); user rows
/// get the full edit affordances.
#[allow(clippy::too_many_arguments)]
fn preset_row(
    cx: &mut Context,
    i: usize,
    e: BrowserEntry,
    sigs: BrowserSignals,
    shared: EditorCtx,
    browse_open: SyncSignal<bool>,
    last_press: SyncSignal<Option<(usize, Instant)>>,
) {
    let selected = sigs.current.map(move |c: &Option<usize>| *c == Some(i));
    let path_opt: Option<PathBuf> = match &e.source {
        EntrySource::User(p) => Some(p.clone()),
        EntrySource::Factory(_) => None,
    };
    let rename_self: Option<RenameTarget> = path_opt.clone().map(RenameTarget::Preset);
    let renaming_btn = {
        let rs = rename_self.clone();
        sigs.rename_target.map(move |t: &Option<RenameTarget>| {
            rs.as_ref().is_some_and(|r| t.as_ref() == Some(r))
        })
    };
    let not_renaming = renaming_btn.map(|b: &bool| !b);

    let sh_click = shared.clone();
    let folder_for_menu = e.folder.clone();
    let path_for_menu = path_opt.clone();
    let label_for_btn = e.name.clone();
    Button::new(cx, move |cx| {
        Label::new(cx, label_for_btn.clone())
            .class("tg-lbl")
            .hoverable(false)
    })
    .class("browser-row")
    .toggle_class("selected", selected)
    .width(Stretch(1.0))
    .cursor(CursorIcon::Hand)
    .display(not_renaming)
    .on_press_down(move |_cx| {
        let es = sigs.entries.get();
        let Some(entry) = es.get(i) else { return };
        let now = Instant::now();
        let is_double = last_press
            .get()
            .map(|(pi, t)| {
                pi == i
                    && now.duration_since(t)
                        < Duration::from_millis(DOUBLE_PRESS_MS as u64)
            })
            .unwrap_or(false);
        if is_double {
            load_entry(
                i,
                &es,
                &sh_click,
                sigs.current,
                sigs.selected_folder,
                sigs.name_field,
            );
            browse_open.set(false);
            last_press.set(None);
        } else {
            select_entry(i, entry, sigs.current, sigs.selected_folder, sigs.name_field);
            last_press.set(Some((i, now)));
        }
        close_menu(sigs);
    })
    .on_mouse_down(move |cx, btn| {
        if btn != MouseButton::Right {
            return;
        }
        let Some(p) = path_for_menu.clone() else { return };
        // Anchor in the layout parent's coord system (physical → logical
        // conversion required) — see `folder_row`.
        let pb = cx.cache.get_bounds(cx.parent());
        let m = cx.mouse();
        sigs.context_menu.set(Some(ContextMenu {
            target: MenuTarget::UserPreset {
                path: p,
                folder: folder_for_menu.clone(),
            },
            anchor_x: cx.physical_to_logical(m.cursor_x - pb.x),
            anchor_y: cx.physical_to_logical(m.cursor_y - pb.y),
            submenu_open: false,
        }));
        sigs.delete_confirm.set(None);
    });

    // Context menu for this preset, anchored just below the row.
    if let Some(p) = path_opt.clone() {
        let folder_for_match = e.folder.clone();
        let shared_menu = shared.clone();
        Binding::new(cx, sigs.context_menu, move |cx| {
            let Some(menu) = sigs.context_menu.get() else {
                return;
            };
            let MenuTarget::UserPreset {
                path: menu_path,
                folder: menu_folder,
            } = &menu.target
            else {
                return;
            };
            if menu_path != &p || menu_folder != &folder_for_match {
                return;
            }
            context_menu_view(cx, menu, sigs, shared_menu.clone());
        });
    }

    if let (Some(rt), Some(path)) = (rename_self, path_opt) {
        let rt_match = rt.clone();
        let renaming = sigs
            .rename_target
            .map(move |t: &Option<RenameTarget>| t.as_ref() == Some(&rt_match));
        let initial = e.name.clone();
        Binding::new(cx, renaming, move |cx| {
            if !renaming.get() {
                return;
            }
            let buf = SyncSignal::new(initial.clone());
            let path_for = path.clone();
            let shared_for = shared.clone();
            Textbox::new(cx, buf)
                .class("preset-field")
                .width(Stretch(1.0))
                .focused(true)
                .on_edit(move |_cx, t| buf.set(t))
                .on_submit(move |_cx, t: String, _success| {
                    commit_preset_rename(&shared_for, &path_for, &t, sigs);
                })
                .on_blur(move |_cx| sigs.rename_target.set(None))
                .on_cancel(move |_cx| sigs.rename_target.set(None));
        });
    }
}

/// One row in the context menu. Matches the `pbar-btn` class for visual
/// consistency with the bar buttons.
fn menu_item(
    cx: &mut Context,
    label: &'static str,
    on_click: impl Fn(&mut EventContext) + 'static + Send + Sync,
) {
    Button::new(cx, move |cx| Label::new(cx, label).class("tg-lbl").hoverable(false))
        .class("context-menu-item")
        .width(Stretch(1.0))
        .cursor(CursorIcon::Hand)
        .on_press_down(on_click);
}

/// Delete row in the context menu. Its label flips to "Click to confirm" and
/// gains the `confirm` class when this target is the pending delete, so the
/// confirmation prompt sits at the cursor instead of only in the status line.
fn delete_item(
    cx: &mut Context,
    target: MenuTarget,
    sigs: BrowserSignals,
    commit: impl Fn(BrowserSignals) + 'static + Clone + Send + Sync,
) {
    let target_for_label = target.clone();
    let label = sigs
        .delete_confirm
        .map(move |dc: &Option<DeleteConfirm>| match dc {
            Some(d) if d.target == target_for_label => "Click to confirm".to_string(),
            _ => "Delete".to_string(),
        });
    let target_for_class = target.clone();
    let confirming = sigs
        .delete_confirm
        .map(move |dc: &Option<DeleteConfirm>| matches!(dc, Some(d) if d.target == target_for_class));
    Button::new(cx, move |cx| Label::new(cx, label).class("tg-lbl").hoverable(false))
        .class("context-menu-item")
        .toggle_class("confirm", confirming)
        .width(Stretch(1.0))
        .cursor(CursorIcon::Hand)
        .on_press_down(move |_cx| {
            let commit_inner = commit.clone();
            delete_action(target.clone(), sigs, move |s| commit_inner(s));
        });
}

/// The floating context menu (ADR 0006 §7). User folder targets get Rename +
/// Delete; user preset targets additionally get Move to ▸ (the submenu is
/// gated on `menu.submenu_open` so the click into Move to ▸ flips the parent
/// menu's `context_menu` signal rather than building a nested binding).
fn context_menu_view(
    cx: &mut Context,
    menu: ContextMenu,
    sigs: BrowserSignals,
    shared: EditorCtx,
) {
    let menu_for_body = menu.clone();
    let shared_body = shared.clone();
    VStack::new(cx, move |cx| {
        match menu_for_body.target.clone() {
            MenuTarget::UserFolder(name) => {
                let n_rename = name.clone();
                menu_item(cx, "Rename", move |_cx| {
                    sigs.rename_target
                        .set(Some(RenameTarget::Folder(n_rename.clone())));
                    close_menu(sigs);
                });
                let n_delete = name.clone();
                let shared_del = shared_body.clone();
                delete_item(
                    cx,
                    MenuTarget::UserFolder(n_delete.clone()),
                    sigs,
                    move |sigs_inner| delete_folder(&shared_del, &n_delete, sigs_inner),
                );
            }
            MenuTarget::UserPreset { path, folder } => {
                let p_rename = path.clone();
                menu_item(cx, "Rename", move |_cx| {
                    sigs.rename_target
                        .set(Some(RenameTarget::Preset(p_rename.clone())));
                    close_menu(sigs);
                });
                let p_delete = path.clone();
                let folder_delete = folder.clone();
                let shared_del = shared_body.clone();
                delete_item(
                    cx,
                    MenuTarget::UserPreset {
                        path: p_delete.clone(),
                        folder: folder_delete.clone(),
                    },
                    sigs,
                    move |sigs_inner| delete_preset(&shared_del, &p_delete, sigs_inner),
                );
                // Move to ▸: toggle the submenu open via context_menu signal.
                let menu_for_toggle = menu_for_body.clone();
                menu_item(cx, "Move to \u{25b8}", move |_cx| {
                    let new = ContextMenu {
                        submenu_open: !menu_for_toggle.submenu_open,
                        ..menu_for_toggle.clone()
                    };
                    sigs.context_menu.set(Some(new));
                });

                // Submenu, only when toggled open.
                if menu_for_body.submenu_open {
                    let path_move = path.clone();
                    let folder_current = folder.clone();
                    let shared_move = shared_body.clone();
                    VStack::new(cx, move |cx| {
                        let rows = sigs.folders.get();
                        let targets = move_targets(&rows, &folder_current);
                        if targets.is_empty() {
                            Label::new(cx, "No other folders").class("browser-empty");
                        }
                        for (key, label) in targets {
                            let dest: Option<String> = match key {
                                FolderKey::UserRoot => None,
                                FolderKey::User(n) => Some(n),
                                FolderKey::Factory(_) => continue,
                            };
                            let shared_for = shared_move.clone();
                            let path_for = path_move.clone();
                            menu_item(cx, leak_label(label), move |_cx| {
                                move_preset(&shared_for, &path_for, dest.as_deref(), sigs);
                                close_menu(sigs);
                            });
                        }
                    })
                    .class("context-menu")
                    .position_type(PositionType::Absolute)
                    .left(Pixels(150.0))
                    .top(Pixels(40.0))
                    .width(Pixels(180.0))
                    .height(Auto)
                    .z_index(320);
                }
            }
        }
    })
    .class("context-menu")
    .position_type(PositionType::Absolute)
    // Anchored at the cursor (`anchor_x` / `anchor_y` are the click's offset
    // within the row, which matches what `Absolute` resolves against when the
    // row is the parent). `ignore_clipping` lets the menu escape the
    // ScrollView's clip path near the bottom of the visible area.
    .left(Pixels(menu.anchor_x))
    .top(Pixels(menu.anchor_y))
    .width(Pixels(150.0))
    .height(Auto)
    .z_index(310)
    .ignore_clipping(true);
}

/// Box-leak a String so it can be passed as `&'static str` to `menu_item`. The
/// menu is rebuilt fresh on every open / submenu toggle, so the leaked strings
/// only accumulate at the rate the user opens the menu — bounded in practice
/// by session length. Avoids threading a String through `Fn(&str)` callbacks
/// that vizia's `Label::new` doesn't accept (only `Res<String>` or `&'static`).
fn leak_label(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// First press: queue a delete-confirm against `target`. The menu item's own
/// label flips to "Click to confirm" via its binding on `delete_confirm`, so
/// the confirmation prompt lives inside the popup, not in the preset bar's
/// status line. Second press on the same target inside `DELETE_CONFIRM_MS`
/// runs `commit` (which closes the menu and reseeds).
fn delete_action(
    target: MenuTarget,
    sigs: BrowserSignals,
    commit: impl Fn(BrowserSignals),
) {
    let ready = match sigs.delete_confirm.get() {
        Some(d) => {
            d.target == target && d.at.elapsed() < Duration::from_millis(DELETE_CONFIRM_MS)
        }
        None => false,
    };
    if ready {
        commit(sigs);
        close_menu(sigs);
    } else {
        sigs.delete_confirm.set(Some(DeleteConfirm {
            target,
            at: Instant::now(),
        }));
    }
}

/// The floating two-pane browser panel (ADR 0006 §1–§4). Built fresh each time
/// the user opens it; rebuilt on the inner `entries` / `folders` signals so a
/// Save or New Folder repopulates without manual invalidation.
#[allow(clippy::too_many_arguments)]
fn browser_panel(
    cx: &mut Context,
    shared: EditorCtx,
    folders: SyncSignal<Arc<Vec<FolderRow>>>,
    entries: SyncSignal<Arc<Vec<BrowserEntry>>>,
    name: SyncSignal<String>,
    status: SyncSignal<String>,
    current: SyncSignal<Option<usize>>,
    browse_open: SyncSignal<bool>,
    selected_folder: SyncSignal<FolderKey>,
    search: SyncSignal<String>,
    name_field: SyncSignal<String>,
    last_press: SyncSignal<Option<(usize, Instant)>>,
    rename_target: SyncSignal<Option<RenameTarget>>,
    context_menu: SyncSignal<Option<ContextMenu>>,
    delete_confirm: SyncSignal<Option<DeleteConfirm>>,
) {
    let sigs = BrowserSignals {
        folders,
        entries,
        current,
        selected_folder,
        name_field,
        name,
        status,
        rename_target,
        context_menu,
        delete_confirm,
    };
    let shared_left = shared.clone();
    let shared_pre = shared.clone();

    VStack::new(cx, move |cx| {
        // ── Search row ──
        HStack::new(cx, move |cx| {
            Textbox::new(cx, search)
                .class("preset-field")
                .width(Stretch(1.0))
                .on_edit(move |_cx, text| search.set(text));
            Button::new(cx, |cx| Label::new(cx, "x").class("tg-lbl"))
                .class("pbar-btn")
                .cursor(CursorIcon::Hand)
                .on_press_down(move |_cx| search.set(String::new()));
        })
        .class("browser-search")
        .horizontal_gap(Pixels(4.0))
        .alignment(Alignment::Center);

        // ── Two-pane folders | presets ──
        let sh_right = shared_pre.clone();
        HStack::new(cx, move |cx| {
            // Left: folder list.
            ScrollView::new(cx, move |cx| {
                Binding::new(cx, folders, move |cx| {
                    let rows = folders.get();
                    for r in rows.iter() {
                        match r {
                            FolderRow::Header(label) => {
                                Label::new(cx, label.clone())
                                    .class("browser-section")
                                    .height(Pixels(22.0));
                            }
                            FolderRow::Folder { key, label } => {
                                folder_row(cx, key.clone(), label.clone(), sigs, shared_left.clone());
                            }
                        }
                    }
                });
            })
            .class("browser-pane")
            .width(Pixels(220.0))
            .height(Stretch(1.0));

            // Right: presets in the selected folder, narrowed by search.
            ScrollView::new(cx, move |cx| {
                Binding::new(cx, entries, move |cx| {
                    let sh_right = sh_right.clone();
                    Binding::new(cx, selected_folder, move |cx| {
                        let sh_right = sh_right.clone();
                        Binding::new(cx, search, move |cx| {
                            let es = entries.get();
                            let folder = selected_folder.get();
                            let query = parse_search(&search.get());
                            let mut shown = 0usize;
                            for (i, e) in es.iter().enumerate() {
                                if e.folder != folder {
                                    continue;
                                }
                                if !query.matches(e) {
                                    continue;
                                }
                                shown += 1;
                                preset_row(
                                    cx,
                                    i,
                                    e.clone(),
                                    sigs,
                                    sh_right.clone(),
                                    browse_open,
                                    last_press,
                                );
                            }
                            if shown == 0 {
                                Label::new(cx, "No presets").class("browser-empty");
                            }
                        });
                    });
                });
            })
            .class("browser-pane")
            .width(Stretch(1.0))
            .height(Stretch(1.0));
        })
        .height(Pixels(240.0))
        .horizontal_gap(Pixels(6.0));

        // ── Save form ──
        let sh_save = shared.clone();
        let sh_new = shared.clone();
        let sh_load = shared.clone();
        VStack::new(cx, move |cx| {
            let save_disabled = selected_folder.map(|f: &FolderKey| f.is_factory());

            // Name input.
            HStack::new(cx, move |cx| {
                Label::new(cx, "Name:").class("browser-saveform-label");
                Textbox::new(cx, name_field)
                    .class("preset-field")
                    .width(Stretch(1.0))
                    .disabled(save_disabled)
                    .on_edit(move |_cx, text| name_field.set(text));
            })
            .horizontal_gap(Pixels(4.0))
            .height(Pixels(20.0))
            .alignment(Alignment::Center);

            // Save / New Folder / Load row.
            HStack::new(cx, move |cx| {
                Button::new(cx, |cx| Label::new(cx, "Save").class("tg-lbl"))
                    .class("pbar-btn")
                    .cursor(CursorIcon::Hand)
                    .disabled(save_disabled)
                    .on_press_down(move |_cx| {
                        let folder_key = selected_folder.get();
                        let Some(folder_arg) = folder_key.save_target() else {
                            status.set("Cannot save into factory".to_string());
                            return;
                        };
                        let name_text = name_field.get();
                        let trimmed = name_text.trim();
                        if trimmed.is_empty() {
                            status.set("Name the preset first".to_string());
                            return;
                        }
                        name.set(trimmed.to_string());
                        sh_save.post(UiEvent::SavePreset {
                            name: trimmed.to_string(),
                            folder: folder_arg,
                        });
                    });

                Button::new(cx, |cx| Label::new(cx, "New Folder").class("tg-lbl"))
                    .class("pbar-btn")
                    .cursor(CursorIcon::Hand)
                    .on_press_down(move |_cx| {
                        // The controller picks the unique folder name and
                        // emits PresetCorpusChanged + Status. The view's
                        // existing 0031 hook (focusing the inline-rename
                        // widget on the freshly created folder) needs the
                        // chosen name; we lose that here until the controller
                        // exposes a `FolderCreated { name }` view event.
                        sh_new.post(UiEvent::NewFolder {
                            suggested: "New Folder".to_string(),
                        });
                    });

                Button::new(cx, |cx| Label::new(cx, "Load").class("tg-lbl"))
                    .class("pbar-btn")
                    .cursor(CursorIcon::Hand)
                    .on_press_down(move |_cx| {
                        let es = entries.get();
                        match current.get() {
                            Some(idx) => {
                                load_entry(
                                    idx,
                                    &es,
                                    &sh_load,
                                    current,
                                    selected_folder,
                                    name_field,
                                );
                                browse_open.set(false);
                            }
                            None => status.set("Select a preset first".to_string()),
                        }
                    });
            })
            .horizontal_gap(Pixels(6.0))
            .height(Pixels(20.0))
            .alignment(Alignment::Left);
        })
        .class("browser-saveform")
        .vertical_gap(Pixels(4.0))
        .height(Auto);

        // ── Outside-click dismissal (ADR 0006 §7) ──
        // The menu itself is rendered inside the right-clicked row (so it
        // anchors correctly). This overlay covers the rest of the panel so a
        // click anywhere else dismisses the menu. The menu's higher z_index
        // (and `ignore_clipping`) puts it above this overlay.
        Binding::new(cx, context_menu, move |cx| {
            if context_menu.get().is_none() {
                return;
            }
            Element::new(cx)
                .position_type(PositionType::Absolute)
                .top(Pixels(0.0))
                .left(Pixels(0.0))
                .width(Stretch(1.0))
                .height(Stretch(1.0))
                .z_index(300)
                .on_press_down(move |_cx| close_menu(sigs));
        });
    })
    .class("browser-panel")
    .position_type(PositionType::Absolute)
    .top(Pixels(22.0))
    .left(Pixels(0.0))
    .width(Pixels(560.0))
    .height(Auto)
    .vertical_gap(Pixels(4.0))
    .z_index(200);
}

/// The "Keys" panel: key-mode selector, Upper/Lower edit-target toggle (hidden
/// in Whole), and split-point control (shown in Split). The mode and split write
/// the **non-automatable** shared state directly (ADR 0003 §3/§8) — not param
/// gestures — so they neither echo to the host as automation nor record a knob
/// move; the edit toggle is pure view state.
fn keys_panel(
    cx: &mut Context,
    shared: &EditorCtx,
    edit_layer: SyncSignal<usize>,
    key_mode: SyncSignal<usize>,
    split: SyncSignal<f32>,
) {
    const MODES: [&str; 3] = ["Whole", "Dual", "Split"];
    const EDIT: [&str; 2] = ["Upper", "Lower"];
    let keys_handle = VStack::new(cx, |cx| {
        Label::new(cx, up("Keys"))
            .class("panel-header")
            .width(Stretch(1.0))
            .height(Pixels(16.0))
            .alignment(Alignment::Center);
        VStack::new(cx, move |cx| {
            // Key-mode selector on the left; the Upper/Lower edit toggle and (under
            // it) the split-point control stacked on the right. Both are hidden
            // until a multi-layer mode reveals them.
            let sh_mode = shared.clone();
            HStack::new(cx, move |cx| {
                // Mode list. Choosing Whole snaps the edit target back to Upper (the
                // toggle is hidden), so we never edit a hidden Lower.
                VStack::new(cx, move |cx| {
                    for (n, label) in MODES.iter().enumerate() {
                        let sh = sh_mode.clone();
                        toggle_row(
                            cx,
                            label,
                            key_mode.map(move |m: &usize| *m == n),
                            move |_cx| {
                                key_mode.set(n);
                                if n == 0 {
                                    edit_layer.set(0);
                                }
                                sh.set_key_mode_seeded(KeyMode::from_u8(n as u8));
                            },
                        );
                    }
                })
                .class("tg-list")
                .height(Auto);

                // Upper/Lower edit toggle: always shown, greyed + inert in Whole
                // (only Dual / Split have a distinct Lower layer to edit).
                Binding::new(cx, key_mode, move |cx| {
                    let enabled = key_mode.get() != 0;
                    VStack::new(cx, move |cx| {
                        for (n, label) in EDIT.iter().enumerate() {
                            gated_toggle_row(
                                cx,
                                label,
                                enabled,
                                edit_layer.map(move |l: &usize| *l == n),
                                move |_cx| edit_layer.set(n),
                            );
                        }
                    })
                    .class("tg-list")
                    .height(Auto);
                });
            })
            .height(Auto)
            .horizontal_gap(Pixels(12.0))
            .alignment(Alignment::TopLeft);

            // Split point — always shown, spanning the full panel width below the
            // mode/edit rows, but greyed + non-interactive unless the Split key mode
            // is selected. A horizontal slider over the MIDI range; the note name
            // shows as a tooltip pinned to the pointer's X, hovering above the
            // slider, on hover or drag. Writes the opaque split state.
            let (hover, drag, show, posx) = (
                SyncSignal::new(false),
                SyncSignal::new(false),
                SyncSignal::new(false),
                SyncSignal::new(0.0f32),
            );
            let (sh_change, sh_dbl) = (shared.clone(), shared.clone());
            Binding::new(cx, key_mode, move |cx| {
                let enabled = key_mode.get() == 2;
                let (sh_change, sh_dbl) = (sh_change.clone(), sh_dbl.clone());
                let col = VStack::new(cx, move |cx| {
                    Slider::new(
                        cx,
                        split.map(|n: &f32| {
                            ((*n - SPLIT_MIN) / (SPLIT_MAX - SPLIT_MIN)).clamp(0.0, 1.0)
                        }),
                    )
                    .width(Stretch(1.0))
                    .height(Pixels(14.0))
                    .cursor(CursorIcon::Hand)
                    .disabled(!enabled)
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
                .height(Auto);
                if !enabled {
                    col.class("dimmed");
                }
            });

            // Reset the patch(es) currently being edited to plain defaults. In
            // Whole the two layers share one patch, so reset both; in Dual/Split
            // reset only the layer the edit toggle points at. Globals, key mode and
            // split point are setup state, left untouched. The `on_idle` poll
            // re-syncs every control signal from the store afterwards.
            let sh_reset = shared.clone();
            Button::new(cx, |cx| Label::new(cx, up("Reset")).class("tg-lbl"))
                .class("reset-btn")
                .width(Stretch(1.0))
                .cursor(CursorIcon::Hand)
                // PressDown, not Press: vizia drops Press if the cursor drifts off
                // the button's rect between down and up (no click slop), so a small
                // wobble eats the click. Firing on down sidesteps that entirely.
                .on_press_down(move |_cx| {
                    if key_mode.get() == 0 {
                        sh_reset.reset_patch_to_defaults(Layer::Upper);
                        sh_reset.reset_patch_to_defaults(Layer::Lower);
                    } else {
                        let layer = if edit_layer.get() == 0 {
                            Layer::Upper
                        } else {
                            Layer::Lower
                        };
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
    probe_id!(keys_handle, "vxn:KeysPanel");
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
    shared: &EditorCtx,
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
                    let sh = shared.clone();
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
                let cells = if header_switch {
                    &entries[1..]
                } else {
                    entries
                };
                for (id, short) in cells {
                    if in_bottom_strip(*id) {
                        continue;
                    }
                    let cid = resolve(*id, layer);
                    // Legato is drawn in the Detune cell's 4th row, not its own column.
                    if matches!(param_ref(cid), Some(ParamRef::Patch(_, PatchParam::Legato))) {
                        continue;
                    }
                    let ctl = controls.iter().copied().find(|c| c.idx() == cid).unwrap();
                    // The Voice panel's Detune fader carries the Legato toggle beneath
                    // it (under the fader, level with the Solo selector's row).
                    if matches!(
                        param_ref(cid),
                        Some(ParamRef::Patch(_, PatchParam::UnisonDetune))
                    ) {
                        detune_legato_cell(cx, ctl, layer, controls, shared, short);
                    } else if matches!(
                        param_ref(cid),
                        Some(ParamRef::Global(GlobalParam::LimiterOn))
                    ) {
                        // The limiter is a bare on/off option (a companion to Volume),
                        // not a headed cell: its box sits with the "LIMIT" label beside
                        // it and no column header above.
                        limiter_cell(cx, ctl, shared, short);
                    } else {
                        control_view(cx, ctl, shared, short);
                    }
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
        // below the main row (frees a horizontal column up top). Absolutely
        // placed so it sits in that empty space without growing the panel.
        //
        // Sized to its content (`width: Auto`) so the stretched-empty-area
        // overlap that used to eat clicks on the column above (Notch on the
        // Filter panel, Lin/Exp ADSR shape rows on the envelopes, etc.) is
        // avoided structurally. Previously this used `right: Stretch(1)` plus
        // `.hoverable(false)` on the container; the latter propagates to
        // children in this vizia rev, killing the cells' clicks too — which
        // is the regression that left the whole strip row dead.
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
            .top(Stretch(1.0))
            .bottom(Pixels(7.0))
            .width(Auto)
            .height(Auto)
            .horizontal_gap(Pixels(12.0));
        }
    })
    .class("panel")
    .height(Pixels(PANEL_H))
    .padding(Pixels(5.0))
    .vertical_gap(Pixels(4.0));
    // Per-panel width share. Panels otherwise stretch equally across a row; the
    // five-control Osc panels take a bigger share, the single-fader Bend panel is
    // pinned narrow, and the row-2 envelopes / LFO 2 are slimmed to free room for
    // the noise (Mixer) and VCA-mod controls.
    let handle = match title {
        "Bend" => handle.width(Pixels(54.0)),
        "Osc 1" | "Osc 2" => handle.width(Stretch(1.2)),
        "LFO 1" => handle.width(Stretch(1.2)),
        "LFO 2" => handle.width(Stretch(0.7)),
        "Mixer" => handle.width(Stretch(1.1)),
        "Env 1" | "Env 2" => handle.width(Stretch(0.8)),
        "VCA" => handle.width(Stretch(0.75)),
        "Filter" => handle.width(Stretch(1.15)),
        _ => handle,
    };
    #[cfg(feature = "layout-probe")]
    let handle = {
        let suffix = if display.is_some() {
            match layer {
                Layer::Upper => ":Upper",
                Layer::Lower => ":Lower",
            }
        } else {
            ""
        };
        handle.id(format!("vxn:Panel::{title}{suffix}"))
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
    // Only build the readout *while shown*, rather than keeping a hidden label whose
    // text is bound to the live value. vizia's `text` modifier calls `needs_relayout`
    // unconditionally on every text change (modifiers/text.rs), ignoring `display` —
    // so a permanently-bound popup relayouts the whole tree each time an automated
    // value's formatted string changes (even at Display::None), and vizia's
    // post-relayout synthetic mouse-move then stomps interaction on other controls.
    // A `Binding` node is layout-ignored, so the label's absolute positioning is
    // unchanged; it just stops existing (and stops relayouting) when hidden.
    Binding::new(cx, show, move |cx| {
        if !show.get() {
            return;
        }
        Label::new(cx, text.clone())
            .class("value-pop")
            .position_type(PositionType::Absolute)
            .top(posy.map(|y: &f32| Pixels(*y)))
            .left(Stretch(1.0))
            .right(Stretch(1.0))
            .width(Auto)
            .height(Auto)
            // Nudge sideways (faders) so the readout sits beside the thumb rather
            // than on top of it, keeping the thumb visible while dragging.
            .translate((Pixels(x_off), Pixels(0.0)))
            .z_index(100)
            .hoverable(false);
    });
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
    // PressDown, not Press: vizia emits Press only if the cursor is still over the
    // same entity at release (no click slop), so a few px of drift on these small
    // rows silently eats the click. Commit on down instead.
    .on_press_down(press);
}

/// A [`toggle_row`] that greys out (`dimmed`) and swallows its click when
/// `enabled` is false — for controls that are only meaningful in certain modes
/// (Legato in the mono assign modes; the Upper/Lower edit toggle outside Whole).
/// Built inside a `Binding` on the gating signal so `enabled` is re-evaluated and
/// the row rebuilt on each mode change.
fn gated_toggle_row(
    cx: &mut Context,
    label: &'static str,
    enabled: bool,
    active: impl Res<bool> + Copy + 'static,
    press: impl Fn(&mut EventContext) + Send + Sync + 'static,
) {
    let tb = ToggleButton::new(cx, active, move |cx| {
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
    .cursor(if enabled {
        CursorIcon::Hand
    } else {
        CursorIcon::Arrow
    })
    .on_press_down(move |cx| {
        if enabled {
            press(cx);
        }
    });
    if !enabled {
        tb.class("dimmed");
    }
}

/// Logical thumb height (was the `.fader .thumb` CSS height); used both to draw
/// the thumb and to inset the drag mapping so the thumb tracks the pointer without
/// running off either end of the track.
const FADER_THUMB_H: f32 = 8.0;

/// A vertical fader drawn entirely in [`View::draw`] (track, fill, thumb) from its
/// bound normalized value.
///
/// Why not vizia's built-in [`Slider`]: its fill is a `Percentage`-sized child, so
/// every value change relayouts the whole tree. Under a continuous host-automation
/// stream (a DAW LFO) that relayout fires every frame, and vizia injects a
/// synthetic mouse-move after each relayout to refresh hover (`systems/layout.rs`),
/// which stamps over any in-progress gesture on other controls — the faceplate goes
/// dead while a value streams. Drawing the fill makes a value change a *redraw*, not
/// a relayout, so the rest of the UI stays interactive.
///
/// The drag/gesture/popup wiring stays on the caller via generic action modifiers
/// (`on_mouse_down`, `on_hover`, `on_double_click`, …); only the value mapping and
/// painting live here. `on_change` fires on press (jump-to-click) and on drag.
///
/// A fader built `disabled` (a greyed-out, inert route — e.g. a Cross Mod depth
/// whose selector is Off) freezes its drawn position to the value at build time and
/// skips automation redraws, so host automation doesn't animate a control that does
/// nothing. The Cross Mod faders are rebuilt when their selector toggles, so the
/// frozen/live choice is re-made (and the thumb snaps to the live value) on re-enable.
struct Fader {
    value: SyncSignal<f32>,
    /// `Some(v)` when built disabled: draw `v` and ignore the live value. `None`
    /// when live (draw tracks `value`).
    frozen: Option<f32>,
    is_dragging: bool,
    on_change: Box<dyn Fn(&mut EventContext, f32)>,
}

impl Fader {
    fn new(
        cx: &mut Context,
        value: SyncSignal<f32>,
        disabled: bool,
        on_change: impl Fn(&mut EventContext, f32) + 'static,
    ) -> Handle<'_, Self> {
        let frozen = disabled.then(|| value.get());
        let handle = Self {
            value,
            frozen,
            is_dragging: false,
            on_change: Box::new(on_change),
        }
        .build(cx, |_| {});
        // Redraw (not relayout) on value change — but only when live. A disabled
        // fader ignores automation entirely, so it never asks for a redraw.
        if frozen.is_none() {
            handle.bind(value, |mut handle| handle.needs_redraw())
        } else {
            handle
        }
    }

    /// Cursor Y → normalized `[0, 1]`, inset by half the thumb at each end so the
    /// thumb centre tracks the pointer without clipping past the track.
    fn value_from_cursor(&self, cx: &EventContext) -> f32 {
        let b = cx.bounds();
        let thumb = FADER_THUMB_H * cx.scale_factor();
        let travel = b.h - thumb;
        if travel <= 0.0 {
            return 0.0;
        }
        ((b.h - (cx.mouse().cursor_y - b.y) - thumb / 2.0) / travel).clamp(0.0, 1.0)
    }
}

impl View for Fader {
    fn element(&self) -> Option<&'static str> {
        Some("fader")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                if cx.is_disabled() {
                    return;
                }
                self.is_dragging = true;
                cx.capture();
                let v = self.value_from_cursor(cx);
                (self.on_change)(cx, v);
            }
            WindowEvent::MouseMove(_, _) => {
                if self.is_dragging && !cx.is_disabled() {
                    let v = self.value_from_cursor(cx);
                    (self.on_change)(cx, v);
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.is_dragging {
                    self.is_dragging = false;
                    cx.release();
                }
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        // Draw at full alpha: vizia wraps a dimmed subtree (`.dimmed`, the disabled
        // Cross Mod columns) in an opacity `save_layer`, so applying opacity here too
        // would double-dim.
        let b = cx.bounds();
        let s = cx.scale_factor();
        // Frozen (disabled) faders draw their build-time value so automation can't
        // animate an inert control even if something else triggers a redraw.
        let n = self
            .frozen
            .unwrap_or_else(|| self.value.get())
            .clamp(0.0, 1.0);

        let track_w = 6.0 * s;
        let center_x = b.x + b.w / 2.0;
        let track_x = center_x - track_w / 2.0;
        let thumb_h = FADER_THUMB_H * s;
        let thumb_top = b.y + (1.0 - n) * (b.h - thumb_h);
        let fill_top = (thumb_top + thumb_h / 2.0).min(b.y + b.h);

        let mut paint = vg::Paint::default();
        paint.set_anti_alias(true);

        // Track (full height), then the active fill rising from the bottom to the thumb.
        paint.set_color(vg::Color::from_rgb(0x55, 0x55, 0x55));
        canvas.draw_round_rect(
            vg::Rect::from_xywh(track_x, b.y, track_w, b.h),
            2.0 * s,
            2.0 * s,
            &paint,
        );
        paint.set_color(vg::Color::from_rgb(0x3a, 0x86, 0xcc));
        canvas.draw_round_rect(
            vg::Rect::from_xywh(track_x, fill_top, track_w, b.y + b.h - fill_top),
            2.0 * s,
            2.0 * s,
            &paint,
        );

        // Thumb spans the full cell width and stays *within* the view bounds. A
        // value change only dirties the view's own bounds, so a wider thumb that
        // overhung the cell left an un-repainted 1px sliver on each side as it slid
        // (the leftover trail). The border is drawn inset by half its width too, so
        // the centred stroke can't bleed past the bounds at the very ends either.
        let bw = 1.0 * s;
        let thumb_rect = vg::Rect::from_xywh(b.x, thumb_top, b.w, thumb_h);
        paint.set_color(vg::Color::from_rgb(0xe8, 0xe8, 0xe8));
        canvas.draw_round_rect(thumb_rect, 1.0 * s, 1.0 * s, &paint);
        let border_rect =
            vg::Rect::from_xywh(b.x + bw / 2.0, thumb_top + bw / 2.0, b.w - bw, thumb_h - bw);
        paint.set_color(vg::Color::from_rgb(0x14, 0x14, 0x14));
        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_width(bw);
        canvas.draw_round_rect(border_rect, 1.0 * s, 1.0 * s, &paint);
    }
}

/// The vertical fader + its hover/drag value popup, without any label — shared by
/// a plain control cell and a mod-route column (where the column header labels it).
/// `disabled` (a build-time bool) blocks the [`Fader`]'s drag (it guards on
/// `cx.is_disabled()`). The Cross Mod depth faders pass this from inside a
/// [`Binding`] on their selector, so the column is rebuilt — and re-disabled or
/// re-enabled — whenever the selector leaves/returns to Off.
fn fader_body(
    cx: &mut Context,
    i: usize,
    sig: SyncSignal<f32>,
    shared: &EditorCtx,
    disabled: bool,
) {
    let (hover, drag, show, posy) = (
        SyncSignal::new(false),
        SyncSignal::new(false),
        SyncSignal::new(false),
        SyncSignal::new(0.0f32),
    );
    let (sh_set, sh_down, sh_up, sh_dbl) = (
        shared.clone(),
        shared.clone(),
        shared.clone(),
        shared.clone(),
    );
    let fader = Fader::new(cx, sig, disabled, move |_cx, v| {
        sig.set(v);
        sh_set.set(i, fader_from_ui_dyn(i, v, &sh_set));
    })
    .cursor(CursorIcon::Hand)
    .width(Pixels(16.0))
    .height(Pixels(FADER_H))
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
    fader.disabled(disabled);
    // A synced LFO rate reads as a musical subdivision; otherwise the descriptor's
    // own display (Hz, st, …). `sync_partner` is `None` for every non-rate fader,
    // so this collapses to the plain path.
    let sh_pop = shared.clone();
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
                    return vxn_app::sync::SUBDIVISIONS[vxn_app::sync::index_from_norm(pos)]
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
    use PatchParam::{
        AmpEnvBypass, Env1Shape, Env2Shape, FilterKeyTrack, FilterSlope, Lfo1FreeRun, LfoSync,
        NoiseColor,
    };
    matches!(
        param_ref(idx),
        Some(ParamRef::Patch(
            _,
            LfoSync
                | Lfo1FreeRun
                | Env1Shape
                | Env2Shape
                | FilterSlope
                | FilterKeyTrack
                | NoiseColor
                | AmpEnvBypass
        )) | Some(ParamRef::Global(
            GlobalParam::Lfo2Sync
                | GlobalParam::DelaySync
                | GlobalParam::DelayPingPong
                | GlobalParam::Oversample
        ))
    )
}

/// One bottom-strip control laid out **horizontally**: a plain bool is a single
/// labelled box; a small enum (Lin/Exp, 12/24 dB, the oversample modes) is a
/// row of exclusive labelled boxes. The vertical [`enum_list_body`] equivalent for
/// the strip.
fn strip_cell(cx: &mut Context, ctl: Ctl, short: &'static str, shared: &EditorCtx) {
    match ctl {
        Ctl::Switch(i, sig) => match desc_for_clap_id(i).unwrap().kind {
            // Two-state enum (Lin/Exp, 12/24 dB): both option boxes in a row.
            ParamKind::Enum { variants } => {
                let sh = shared.clone();
                HStack::new(cx, move |cx| {
                    for (n, label) in variants.iter().enumerate() {
                        let sh = sh.clone();
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
                .width(Auto)
                .height(Auto)
                .horizontal_gap(Pixels(6.0));
            }
            // Plain bool: a single box labelled with the control's short name.
            _ => {
                let sh = shared.clone();
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
            let sh = shared.clone();
            HStack::new(cx, move |cx| {
                for (n, label) in variants.iter().enumerate() {
                    let sh = sh.clone();
                    toggle_row(
                        cx,
                        label,
                        sig.map(move |s: &Option<usize>| *s == Some(n)),
                        move |_cx| {
                            sig.set(Some(n));
                            sh.set(i, n as f32);
                        },
                    );
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
    shared: &EditorCtx,
) {
    let variants = match desc_for_clap_id(i).unwrap().kind {
        ParamKind::Enum { variants } => variants,
        _ => &[],
    };
    // Display order: natural enum order, except the assign modes read Poly, Twin,
    // Unison, Solo (Twin sits by Poly as the other "thin" mode) — a view reorder
    // only; the stored value is still each variant's own index.
    let order: Vec<usize> = if matches!(
        param_ref(i),
        Some(ParamRef::Patch(_, PatchParam::AssignMode))
    ) {
        ASSIGN_DISPLAY_ORDER.to_vec()
    } else {
        (0..variants.len()).collect()
    };
    let sh = shared.clone();
    VStack::new(cx, move |cx| {
        for n in order {
            let label = variants[n];
            let sh = sh.clone();
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

fn control_view(cx: &mut Context, ctl: Ctl, shared: &EditorCtx, short: &'static str) {
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
                    (shared.clone(), shared.clone(), shared.clone());
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
                    // A named two-state enum (12/24 dB, Linear/Exponential):
                    // an exclusive two-row list so the state name stays visible.
                    ParamKind::Enum { variants } => {
                        let sh = shared.clone();
                        VStack::new(cx, move |cx| {
                            for (n, label) in variants.iter().enumerate() {
                                let sh = sh.clone();
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
                        let sh = shared.clone();
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

/// The Voice panel's Detune column, carrying the **Legato** toggle in its 4th row
/// (beneath the fader, level with the Solo selector in the assign column). The
/// fader spans the first three rows; a fixed-height content box (matching the
/// assign list's 4-row height) holds the fader on top and the toggle at the bottom,
/// so this column stays the same width as a plain fader column. Legato applies to
/// the mono modes (Solo / Unison); it greys out (but stays present/automatable)
/// in Poly / Twin.
fn detune_legato_cell(
    cx: &mut Context,
    detune: Ctl,
    layer: Layer,
    controls: &[Ctl],
    shared: &EditorCtx,
    short: &'static str,
) {
    let Ctl::Fader(fi, fsig) = detune else {
        return;
    };
    let legato = controls
        .iter()
        .copied()
        .find(|c| c.idx() == patch_clap_id(layer, PatchParam::Legato));
    // Legato is only meaningful in the mono assign modes; bind on the assign
    // selector so the toggle greys out in Poly / Twin.
    let assign_sig = controls
        .iter()
        .copied()
        .find(|c| c.idx() == patch_clap_id(layer, PatchParam::AssignMode))
        .and_then(|c| match c {
            Ctl::Buttons(_, s) | Ctl::Select(_, s) => Some(s),
            _ => None,
        });
    // Content box height = the assign list's 4 rows (4 × 24px + 3 × 1px gap), so the
    // toggle's bottom row lines up with the Solo selector. Fader (74) + 1px gap +
    // toggle (24) = 99, exactly that height.
    const LIST_H: f32 = 4.0 * 24.0 + 3.0 * 1.0;
    let sh = shared.clone();
    let shf = shared.clone();
    VStack::new(cx, move |cx| {
        Label::new(cx, up(short))
            .class("ctl-label")
            .height(Pixels(11.0));
        VStack::new(cx, move |cx| {
            fader_body(cx, fi, fsig, &shf, false);
            if let (Some(Ctl::Switch(li, lsig)), Some(asig)) = (legato, assign_sig) {
                let sh2 = sh.clone();
                Binding::new(cx, asig, move |cx| {
                    // Unison = 1, Solo = 2 (the mono modes); grey out in Poly / Twin.
                    let enabled = matches!(asig.get(), Some(1) | Some(2));
                    let sh3 = sh2.clone();
                    gated_toggle_row(cx, "Legato", enabled, lsig, move |_cx| {
                        let v = !lsig.get();
                        lsig.set(v);
                        sh3.set(li, if v { 1.0 } else { 0.0 });
                    });
                });
            }
        })
        .height(Pixels(LIST_H))
        .vertical_gap(Pixels(1.0))
        .alignment(Alignment::TopCenter);
    })
    .height(Pixels(COL_H))
    .vertical_gap(Pixels(10.0))
    .alignment(Alignment::TopCenter);
}

/// The Master panel's limiter toggle, rendered as a bare on/off option rather
/// than a headed cell: a single toggle box with the "LIMIT" label beside it (an
/// "on"-style companion to the Volume fader), no column header above. An empty
/// header-height label keeps it vertically aligned with the headed cells beside it.
fn limiter_cell(cx: &mut Context, ctl: Ctl, shared: &EditorCtx, short: &'static str) {
    let Ctl::Switch(i, sig) = ctl else {
        return;
    };
    let sh = shared.clone();
    VStack::new(cx, move |cx| {
        // No header text; keep the slot so the toggle lines up with neighbour cells.
        Label::new(cx, "").class("ctl-label").height(Pixels(11.0));
        toggle_row(cx, short, sig, move |_cx| {
            let on = !sig.get();
            sig.set(on);
            sh.set(i, if on { 1.0 } else { 0.0 });
        });
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
    shared: &EditorCtx,
) {
    let find = |id: usize| {
        controls
            .iter()
            .copied()
            .find(|c| c.idx() == resolve(id, layer))
            .unwrap()
    };
    VStack::new(cx, |cx| {
        Label::new(cx, up(header))
            .class("ctl-label")
            .height(Pixels(11.0));
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
fn lfo1_cells(cx: &mut Context, layer: Layer, controls: &[Ctl], shared: &EditorCtx) {
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
fn cross_mod_panel(cx: &mut Context, layer: Layer, controls: &[Ctl], shared: &EditorCtx) {
    use PatchParam::*;
    xmod_pair(
        cx,
        "Type",
        CrossModType,
        "Amt",
        CrossModAmount,
        layer,
        controls,
        shared,
    );
    xmod_pair(
        cx,
        "Src",
        Osc2PitchEnvSrc,
        "Mod",
        Osc2PitchEnvDepth,
        layer,
        controls,
        shared,
    );
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
    shared: &EditorCtx,
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
            Label::new(cx, up(sel_label))
                .class("ctl-label")
                .height(Pixels(11.0));
            if let Ctl::Buttons(i, sig) | Ctl::Select(i, sig) = sel_ctl {
                enum_list_body(cx, i, sig, shared);
            }
        })
        .height(Auto)
        .vertical_gap(Pixels(6.0))
        .alignment(Alignment::TopCenter);

        if let (Some(sel_sig), Ctl::Fader(fi, fsig)) = (sel_sig, depth_ctl) {
            let sh = shared.clone();
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
                    Label::new(cx, up(depth_label))
                        .class("ctl-label")
                        .height(Pixels(11.0));
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
    use vxn_app::Taper;

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
        assert_eq!(
            sync_partner(patch_clap_id(Layer::Upper, PatchParam::Cutoff)),
            None
        );
        assert_eq!(
            sync_partner(global_clap_id(GlobalParam::MasterVolume)),
            None
        );
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
            (global_clap_id(GlobalParam::Lfo2Rate), &[0.5, 5.0, 40.0]),
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
            assert!(matches!(
                desc_for_clap_id(id).unwrap().taper(),
                Taper::Exp { .. }
            ));
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
        assert!(matches!(
            desc_for_clap_id(id).unwrap().taper(),
            Taper::Exp { .. }
        ));
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

    // ── Preset browser helpers (0027 / 0030) ─────────────────────────────────

    /// Build a factory `PresetMeta` for browser tests (just name + category;
    /// the meta-only fields the corpus needs to display).
    fn fp(category: &str, name: &str) -> PresetMeta {
        PresetMeta {
            name: name.to_string(),
            category: Some(category.to_string()),
            ..Default::default()
        }
    }

    fn up_entry_in(name: &str, folder: Option<&str>) -> UserPresetEntry {
        let parent = folder.unwrap_or("root");
        UserPresetEntry {
            path: PathBuf::from(format!("/tmp/{parent}/{name}.toml")),
            meta: PresetMeta {
                name: name.to_string(),
                ..Default::default()
            },
            folder: folder.map(str::to_string),
        }
    }

    #[test]
    fn step_index_wraps_and_seeds() {
        assert_eq!(step_index(1, None, 0), None); // empty list
        assert_eq!(step_index(1, None, 3), Some(0)); // forward from nothing → first
        assert_eq!(step_index(-1, None, 3), Some(2)); // back from nothing → last
        assert_eq!(step_index(1, Some(2), 3), Some(0)); // wrap forward
        assert_eq!(step_index(-1, Some(0), 3), Some(2)); // wrap back
        assert_eq!(step_index(1, Some(0), 3), Some(1));
    }

    #[test]
    fn build_browser_orders_factory_then_user_in_folder_order() {
        let corpus = PresetCorpus {
            factory: vec![
                fp("Pad", "Glass"),     // idx 0
                fp("Bass", "Mini"),     // idx 1
                fp("Bass", "FM Growl"), // idx 2
            ],
            user: vec![
                UserFolderEntry {
                    name: None,
                    presets: vec![up_entry_in("Loose", None)],
                },
                UserFolderEntry {
                    name: Some("Drums".to_string()),
                    presets: vec![up_entry_in("Kick", Some("Drums"))],
                },
                UserFolderEntry {
                    name: Some("Atmos".to_string()),
                    presets: vec![up_entry_in("Drone", Some("Atmos"))],
                },
            ],
        };
        let (rows, entries) = build_browser_from_corpus(&corpus);

        let row_shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                FolderRow::Header(s) => format!("H:{s}"),
                FolderRow::Folder { label, .. } => format!("F:{label}"),
            })
            .collect();
        assert_eq!(
            row_shape,
            vec![
                "H:Factory".to_string(),
                "F:Bass".to_string(),
                "F:Pad".to_string(),
                "H:User".to_string(),
                format!("F:{UNCATEGORIZED}"),
                "F:Atmos".to_string(),
                "F:Drums".to_string(),
            ]
        );

        let entry_shape: Vec<(String, String)> = entries
            .iter()
            .map(|e| {
                let fk = match &e.folder {
                    FolderKey::Factory(c) => format!("F:{c}"),
                    FolderKey::UserRoot => format!("U:{UNCATEGORIZED}"),
                    FolderKey::User(n) => format!("U:{n}"),
                };
                (fk, e.name.clone())
            })
            .collect();
        assert_eq!(
            entry_shape,
            vec![
                ("F:Bass".into(), "FM Growl".into()),
                ("F:Bass".into(), "Mini".into()),
                ("F:Pad".into(), "Glass".into()),
                (format!("U:{UNCATEGORIZED}"), "Loose".into()),
                ("U:Atmos".into(), "Drone".into()),
                ("U:Drums".into(), "Kick".into()),
            ]
        );
        // Factory indices point back into the unsorted bank.
        assert!(matches!(entries[0].source, EntrySource::Factory(2)));
        assert!(matches!(entries[1].source, EntrySource::Factory(1)));
        assert!(matches!(entries[2].source, EntrySource::Factory(0)));
        // Users carry their on-disk path.
        assert!(matches!(&entries[3].source, EntrySource::User(p) if p.ends_with("Loose.toml")));
    }

    #[test]
    fn build_browser_always_includes_user_root_even_with_no_tree() {
        let corpus = PresetCorpus::default();
        let (rows, entries) = build_browser_from_corpus(&corpus);
        assert!(entries.is_empty());
        let row_shape: Vec<&FolderRow> = rows.iter().collect();
        assert!(matches!(row_shape[0], FolderRow::Header(s) if s == "User"));
        assert!(
            matches!(row_shape[1], FolderRow::Folder { key: FolderKey::UserRoot, label } if label == UNCATEGORIZED)
        );
    }

    #[test]
    fn parse_search_lowercases_and_trims() {
        let q = parse_search("Glass Pad");
        assert_eq!(q.text, "glass pad");

        let q = parse_search("   Mini   ");
        assert_eq!(q.text, "mini");

        let q = parse_search("");
        assert!(q.text.is_empty());
    }

    #[test]
    fn move_targets_list_excludes_current_and_factory() {
        // Hand-built row list mirroring `build_browser`'s output: Factory section
        // (ignored), then the user section with Uncategorised + two subfolders.
        let rows = vec![
            FolderRow::Header("Factory".into()),
            FolderRow::Folder {
                key: FolderKey::Factory("Pad".into()),
                label: "Pad".into(),
            },
            FolderRow::Header("User".into()),
            FolderRow::Folder {
                key: FolderKey::UserRoot,
                label: UNCATEGORIZED.into(),
            },
            FolderRow::Folder {
                key: FolderKey::User("Bass Patches".into()),
                label: "Bass Patches".into(),
            },
            FolderRow::Folder {
                key: FolderKey::User("Leads".into()),
                label: "Leads".into(),
            },
        ];

        // From "Bass Patches": Uncategorised + Leads (factory skipped, self skipped).
        let targets = move_targets(&rows, &FolderKey::User("Bass Patches".into()));
        let labels: Vec<&str> = targets.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, vec![UNCATEGORIZED, "Leads"]);
        // First entry is always Uncategorised when the source isn't the root.
        assert!(matches!(targets[0].0, FolderKey::UserRoot));

        // From Uncategorised: both subfolders, no UserRoot entry.
        let targets = move_targets(&rows, &FolderKey::UserRoot);
        let labels: Vec<&str> = targets.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, vec!["Bass Patches", "Leads"]);

        // Factory rows are never offered as targets even when no user folders
        // exist — the move-to flow is user-only (ADR 0006 §5/§7).
        let factory_only = vec![
            FolderRow::Header("Factory".into()),
            FolderRow::Folder {
                key: FolderKey::Factory("Pad".into()),
                label: "Pad".into(),
            },
            FolderRow::Folder {
                key: FolderKey::Factory("Bass".into()),
                label: "Bass".into(),
            },
        ];
        assert!(
            move_targets(&factory_only, &FolderKey::UserRoot).is_empty(),
            "factory rows must never appear as move targets"
        );
    }

    #[test]
    fn search_query_matches_substring_on_name() {
        let corpus = PresetCorpus {
            factory: vec![
                fp("Pad", "Glass Pad"),
                fp("Pad", "Warm Pad"),
                fp("Bass", "Mini"),
            ],
            user: vec![],
        };
        let (_, entries) = build_browser_from_corpus(&corpus);
        let glass = entries.iter().find(|e| e.name == "Glass Pad").unwrap();
        let warm = entries.iter().find(|e| e.name == "Warm Pad").unwrap();
        let mini = entries.iter().find(|e| e.name == "Mini").unwrap();

        // Substring narrows by name, case-insensitive.
        let q = parse_search("pad");
        assert!(q.matches(glass));
        assert!(q.matches(warm));
        assert!(!q.matches(mini));

        // Multi-word substring matches as a single fragment.
        let q = parse_search("glass pad");
        assert!(q.matches(glass));
        assert!(!q.matches(warm));

        // Empty query matches everything.
        let q = parse_search("");
        for e in entries.iter() {
            assert!(q.matches(e));
        }
    }
}
