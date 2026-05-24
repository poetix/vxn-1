//! VXN1 parameter model — split into a per-patch block and a global block
//! (ADR 0003 §6).
//!
//! A **layer is a complete patch**: oscillators, noise, filter, envelopes, LFO
//! and modulation matrix. Those live in [`PatchParam`] and are instantiated
//! **twice** (Upper, Lower — see [`Layer`]), so each layer is independently
//! automatable. Truly global state (master tune/volume, FX, oversample) lives
//! once in [`GlobalParam`].
//!
//! ## CLAP id layout
//!
//! Every automatable parameter needs a stable integer id. The id space is three
//! contiguous ranges:
//!
//! ```text
//! [ 0 .. PATCH_COUNT )                 Upper per-patch params
//! [ PATCH_COUNT .. 2*PATCH_COUNT )     Lower per-patch params
//! [ 2*PATCH_COUNT .. TOTAL_PARAMS )    global params
//! ```
//!
//! [`patch_clap_id`] / [`global_clap_id`] map a typed param to its id;
//! [`param_ref`] / [`desc_for_clap_id`] invert it for incoming automation and
//! for the CLAP/UI metadata callbacks.
//!
//! `KeyMode` and the split point are **not** in this table: they are
//! non-automatable shared state (ADR 0003 §3, §8), carried as atomics in
//! [`crate::SharedParams`] and persisted by [`crate::state`].
//!
//! Values are stored as `f32` in *plain* units (Hz, seconds, semitones, …),
//! matching CLAP's plain-value convention. Enum/bool params store the variant
//! index / 0.0|1.0 and are read back through typed accessors.
//!
//! The 20 modulation-depth params (`Env1Pitch` … `KeyPwm`) are laid out
//! source-major, destination-minor **within** the per-patch block so the engine
//! can address them by `MATRIX_BASE + source*ModDest::COUNT + dest` (see
//! [`crate::modmatrix`]).

use crate::modmatrix::{ModDest, ModSource};
use vxn_dsp::{AdsrShape, LadderVariant, LfoShape, NoiseColor, Waveform};

/// Which of the two always-present patches a per-patch param belongs to.
/// Discriminant doubles as the index into [`ParamValues::layers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Layer {
    Upper = 0,
    Lower = 1,
}

impl Layer {
    pub const COUNT: usize = 2;
    pub const ALL: [Layer; Self::COUNT] = [Layer::Upper, Layer::Lower];
}

/// Jupiter-8 key mode. Non-automatable shared state (ADR 0003 §3): it travels in
/// the plugin-state blob, not the CLAP param table, because its seed-on-entry
/// side effect (0009) wants a discrete edge rather than an automation value
/// stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum KeyMode {
    #[default]
    Whole = 0,
    Dual = 1,
    Split = 2,
}

impl KeyMode {
    pub const COUNT: usize = 3;
    pub const ALL: [KeyMode; Self::COUNT] = [KeyMode::Whole, KeyMode::Dual, KeyMode::Split];

    pub fn from_u8(v: u8) -> KeyMode {
        match v {
            1 => KeyMode::Dual,
            2 => KeyMode::Split,
            _ => KeyMode::Whole,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KeyMode::Whole => "Whole",
            KeyMode::Dual => "Dual",
            KeyMode::Split => "Split",
        }
    }
}

/// Default split point (MIDI note) when none has been set — middle C.
pub const DEFAULT_SPLIT_POINT: u8 = 60;

/// Per-layer voice-assignment mode (ADR 0003 §4): how one logical note maps to
/// the layer's 8 channels. The per-layer MIDI processor (0010) implements it;
/// unison (0011) stacks all channels with detune, portamento (0012) glides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum AssignMode {
    /// First-free / oldest-steal across the layer's 8 channels (today's poly).
    #[default]
    Poly,
    /// One note stacked across all 8 channels with per-channel detune (0011).
    Unison,
}

impl AssignMode {
    pub fn from_index(i: usize) -> AssignMode {
        match i {
            1 => AssignMode::Unison,
            _ => AssignMode::Poly,
        }
    }
}

/// Per-patch parameter ids. Discriminant = index into the per-patch block (and
/// into [`PATCH_PARAMS`]). Instantiated once per [`Layer`]; the CLAP id is
/// derived via [`patch_clap_id`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum PatchParam {
    // Oscillator 1
    Osc1Wave,
    Osc1Coarse,
    Osc1Fine,
    Osc1Level,
    Osc1PulseWidth,
    // Oscillator 2
    Osc2Wave,
    Osc2Coarse,
    Osc2Fine,
    Osc2Level,
    Osc2PulseWidth,
    // Noise
    NoiseColor,
    NoiseLevel,
    // Filter (ladder VCF)
    Cutoff,
    Resonance,
    Drive,
    FilterVariant,
    // Envelope 1 (assignable — defaults unrouted)
    Env1Attack,
    Env1Decay,
    Env1Sustain,
    Env1Release,
    Env1Shape,
    // Envelope 2 (defaults to the VCA amp envelope)
    Env2Attack,
    Env2Decay,
    Env2Sustain,
    Env2Release,
    Env2Shape,
    // ── Modulation matrix: source-major, dest-minor (Pitch, Cutoff, Amp, Pwm) ──
    Env1Pitch,
    Env1Cutoff,
    Env1Amp,
    Env1Pwm,
    Env2Pitch,
    Env2Cutoff,
    Env2Amp,
    Env2Pwm,
    LfoPitch,
    LfoCutoff,
    LfoAmp,
    LfoPwm,
    VelPitch,
    VelCutoff,
    VelAmp,
    VelPwm,
    KeyPitch,
    KeyCutoff,
    KeyAmp,
    KeyPwm,
    // LFO (per-layer — ADR 0003 §5)
    LfoShape,
    LfoRate,
    // ── Appended after v1 to keep earlier in-block offsets stable (E001) ──
    /// Pre-VCF high-pass cutoff (Hz). 20 ≈ fully open / "off".
    HpfCutoff,
    /// Per-oscillator octave offset, stacks with coarse/fine.
    Osc1Octave,
    Osc2Octave,
    /// Per-voice fade-in of LFO modulation after note-on (s).
    LfoDelay,
    // ── E002: oscillator interaction ──
    /// Hard sync: osc2 (slave) phase resets each osc1 (master) cycle.
    OscSync,
    /// Cross-mod / linear FM depth: osc2 output modulates osc1 pitch.
    CrossMod,
    /// Mod-wheel (CC1) destination: Off / Cutoff / Osc2 Pitch.
    ModWheelDest,
    /// Mod-wheel modulation depth (semitone-domain: cutoff octaves or osc2 st).
    ModWheelDepth,
    // ── E003 (offsets stay stable above this line) ──
    /// Voice-assignment mode for this layer (Poly / Unison — ADR 0003 §4).
    AssignMode,
}

impl PatchParam {
    pub const COUNT: usize = PatchParam::AssignMode as usize + 1;

    /// In-block offset of the first modulation-matrix parameter (`Env1Pitch`).
    pub const MATRIX_BASE: usize = PatchParam::Env1Pitch as usize;

    pub fn all() -> impl Iterator<Item = PatchParam> {
        (0..Self::COUNT).map(|i| Self::from_index(i).unwrap())
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<PatchParam> {
        if i < Self::COUNT {
            Some(unsafe { std::mem::transmute::<usize, PatchParam>(i) })
        } else {
            None
        }
    }

    /// In-block offset where source `src`'s row of destination-depth params
    /// begins. Single source of truth for the matrix layout — [`Self::matrix_index`],
    /// [`Self::is_matrix_param`] and the engine's `build_ctx` all derive from it.
    #[inline]
    pub fn matrix_row_base(src: ModSource) -> usize {
        Self::MATRIX_BASE + (src as usize) * ModDest::COUNT
    }

    /// In-block offset of the depth param for a `(source, destination)` route.
    #[inline]
    pub fn matrix_index(src: ModSource, dest: ModDest) -> usize {
        Self::matrix_row_base(src) + (dest as usize)
    }

    /// Whether in-block offset `idx` is one of the modulation-depth params.
    #[inline]
    pub fn is_matrix_param(idx: usize) -> bool {
        ModSource::ALL.iter().any(|&src| {
            let base = Self::matrix_row_base(src);
            (base..base + ModDest::COUNT).contains(&idx)
        })
    }

    pub fn desc(self) -> &'static ParamDesc {
        &PATCH_PARAMS[self.index()]
    }
}

/// Global parameter ids. Discriminant = index into the global block (and into
/// [`GLOBAL_PARAMS`]); the CLAP id is derived via [`global_clap_id`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum GlobalParam {
    MasterTune,
    MasterVolume,
    // Chorus
    ChorusOn,
    ChorusRate,
    ChorusDepth,
    ChorusMix,
    // Delay
    DelayOn,
    DelayTime,
    DelayFeedback,
    DelayMix,
    DelayPingPong,
    // Quality
    Oversample,
}

impl GlobalParam {
    pub const COUNT: usize = GlobalParam::Oversample as usize + 1;

    pub fn all() -> impl Iterator<Item = GlobalParam> {
        (0..Self::COUNT).map(|i| Self::from_index(i).unwrap())
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<GlobalParam> {
        if i < Self::COUNT {
            Some(unsafe { std::mem::transmute::<usize, GlobalParam>(i) })
        } else {
            None
        }
    }

    pub fn desc(self) -> &'static ParamDesc {
        &GLOBAL_PARAMS[self.index()]
    }
}

// ── CLAP id layout ──────────────────────────────────────────────────────────

/// Number of per-patch params (per layer).
pub const PATCH_COUNT: usize = PatchParam::COUNT;
/// Number of global params.
pub const GLOBAL_COUNT: usize = GlobalParam::COUNT;
/// Total CLAP parameter count: two per-patch blocks plus the global block.
pub const TOTAL_PARAMS: usize = Layer::COUNT * PATCH_COUNT + GLOBAL_COUNT;

/// CLAP id of per-patch param `p` on `layer`.
#[inline]
pub const fn patch_clap_id(layer: Layer, p: PatchParam) -> usize {
    (layer as usize) * PATCH_COUNT + (p as usize)
}

/// CLAP id of global param `g`.
#[inline]
pub const fn global_clap_id(g: GlobalParam) -> usize {
    Layer::COUNT * PATCH_COUNT + (g as usize)
}

/// A resolved CLAP id: either a per-patch param on a specific layer, or global.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamRef {
    Patch(Layer, PatchParam),
    Global(GlobalParam),
}

/// Resolve a CLAP id back to its typed param (inverse of [`patch_clap_id`] /
/// [`global_clap_id`]). `None` if out of range.
pub fn param_ref(clap_id: usize) -> Option<ParamRef> {
    if clap_id < PATCH_COUNT {
        Some(ParamRef::Patch(
            Layer::Upper,
            PatchParam::from_index(clap_id)?,
        ))
    } else if clap_id < Layer::COUNT * PATCH_COUNT {
        Some(ParamRef::Patch(
            Layer::Lower,
            PatchParam::from_index(clap_id - PATCH_COUNT)?,
        ))
    } else if clap_id < TOTAL_PARAMS {
        Some(ParamRef::Global(GlobalParam::from_index(
            clap_id - Layer::COUNT * PATCH_COUNT,
        )?))
    } else {
        None
    }
}

/// Descriptor for a CLAP id (metadata for `get_info` / `value_to_text` / UI).
pub fn desc_for_clap_id(clap_id: usize) -> Option<&'static ParamDesc> {
    match param_ref(clap_id)? {
        ParamRef::Patch(_, p) => Some(p.desc()),
        ParamRef::Global(g) => Some(g.desc()),
    }
}

/// CLAP `module` string for a CLAP id — groups the automation list by layer
/// ("Upper"/"Lower"/"Global") without disturbing the per-control label.
pub fn module_for_clap_id(clap_id: usize) -> &'static str {
    match param_ref(clap_id) {
        Some(ParamRef::Patch(Layer::Upper, _)) => "Upper",
        Some(ParamRef::Patch(Layer::Lower, _)) => "Lower",
        Some(ParamRef::Global(_)) => "Global",
        None => "",
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    Float { unit: &'static str, log: bool },
    Int { unit: &'static str },
    Bool,
    Enum { variants: &'static [&'static str] },
}

/// Pure metadata for one parameter (name, range, formatting). Id-type-agnostic:
/// shared by the per-patch and global tables, addressed positionally.
#[derive(Clone, Copy, Debug)]
pub struct ParamDesc {
    pub name: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub kind: ParamKind,
}

impl ParamDesc {
    #[inline]
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }

    #[inline]
    pub fn to_normalized(&self, v: f32) -> f32 {
        if self.max > self.min {
            ((v - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    #[inline]
    pub fn from_normalized(&self, n: f32) -> f32 {
        self.min + n.clamp(0.0, 1.0) * (self.max - self.min)
    }

    /// Human-readable value text (shared by the CLAP `value_to_text` callback
    /// and the editor's value readouts, so both render identically).
    pub fn display(&self, value: f32) -> String {
        match self.kind {
            ParamKind::Enum { variants } => {
                let i = (value.round() as usize).min(variants.len().saturating_sub(1));
                variants[i].to_string()
            }
            ParamKind::Bool => if value >= 0.5 { "On" } else { "Off" }.to_string(),
            ParamKind::Int { unit } => format!("{} {}", value.round() as i64, unit),
            ParamKind::Float { unit, .. } => {
                if unit.is_empty() {
                    format!("{value:.3}")
                } else {
                    format!("{value:.2} {unit}")
                }
            }
        }
    }
}

const WAVE_LABELS: &[&str] = &["Sine", "Triangle", "Saw", "Pulse"];
const NOISE_LABELS: &[&str] = &["White", "Pink"];
const VARIANT_LABELS: &[&str] = &["Sharp", "Smooth"];
const SHAPE_LABELS: &[&str] = &["Linear", "Exponential"];
const LFO_LABELS: &[&str] = &["Sine", "Tri", "Saw+", "Saw-", "Square", "S&H"];
const OVERSAMPLE_LABELS: &[&str] = &["Off", "2x", "4x"];
const MOD_WHEEL_DEST_LABELS: &[&str] = &["Off", "Cutoff", "Osc2 Pitch"];
const ASSIGN_LABELS: &[&str] = &["Poly", "Unison"];

const fn f(
    name: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: &'static str,
    log: bool,
) -> ParamDesc {
    ParamDesc {
        name,
        label,
        min,
        max,
        default,
        kind: ParamKind::Float { unit, log },
    }
}
const fn e(
    name: &'static str,
    label: &'static str,
    variants: &'static [&'static str],
    default: f32,
) -> ParamDesc {
    ParamDesc {
        name,
        label,
        min: 0.0,
        max: (variants.len() - 1) as f32,
        default,
        kind: ParamKind::Enum { variants },
    }
}
const fn b(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    ParamDesc {
        name,
        label,
        min: 0.0,
        max: 1.0,
        default,
        kind: ParamKind::Bool,
    }
}
const fn i(
    name: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: &'static str,
) -> ParamDesc {
    ParamDesc {
        name,
        label,
        min,
        max,
        default,
        kind: ParamKind::Int { unit },
    }
}
/// Pitch-destination depth param (semitones). `default` lets LFO→Pitch seed a
/// gentle default vibrato.
const fn mp(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(name, label, -48.0, 48.0, default, "st", false)
}
/// Cutoff-destination depth param (semitones of cutoff).
const fn mc(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -96.0, 96.0, 0.0, "st", false)
}
/// Amp-destination depth param (gain). `default` lets ENV-2→Amp seed to 1.0.
const fn ma(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(name, label, -1.0, 1.0, default, "", false)
}
/// PWM-destination depth param (pulse-width fraction).
const fn mw(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -0.5, 0.5, 0.0, "", false)
}

/// Per-patch descriptor table; indexed by [`PatchParam`] (= in-block offset).
pub static PATCH_PARAMS: [ParamDesc; PatchParam::COUNT] = [
    e("osc1_wave", "Osc 1 Wave", WAVE_LABELS, 2.0),
    i("osc1_coarse", "Osc 1 Coarse", -24.0, 24.0, 0.0, "st"),
    f("osc1_fine", "Osc 1 Fine", -50.0, 50.0, 0.0, "ct", false),
    f("osc1_level", "Osc 1 Level", 0.0, 1.0, 0.8, "", false),
    f("osc1_pw", "Osc 1 PW", 0.05, 0.95, 0.5, "", false),
    e("osc2_wave", "Osc 2 Wave", WAVE_LABELS, 2.0),
    i("osc2_coarse", "Osc 2 Coarse", -24.0, 24.0, -12.0, "st"),
    f("osc2_fine", "Osc 2 Fine", -50.0, 50.0, 7.0, "ct", false),
    f("osc2_level", "Osc 2 Level", 0.0, 1.0, 0.6, "", false),
    f("osc2_pw", "Osc 2 PW", 0.05, 0.95, 0.5, "", false),
    e("noise_color", "Noise Color", NOISE_LABELS, 0.0),
    f("noise_level", "Noise Level", 0.0, 1.0, 0.0, "", false),
    f("cutoff", "Cutoff", 20.0, 18000.0, 8000.0, "Hz", true),
    f("resonance", "Resonance", 0.0, 1.0, 0.2, "", false),
    f("drive", "Drive", 0.1, 4.0, 1.0, "", false),
    e("filter_variant", "Filter Type", VARIANT_LABELS, 0.0),
    f("env1_attack", "Env 1 Attack", 0.001, 10.0, 0.005, "s", true),
    f("env1_decay", "Env 1 Decay", 0.001, 10.0, 0.3, "s", true),
    f("env1_sustain", "Env 1 Sustain", 0.0, 1.0, 0.0, "", false),
    f("env1_release", "Env 1 Release", 0.001, 10.0, 0.3, "s", true),
    e("env1_shape", "Env 1 Shape", SHAPE_LABELS, 0.0),
    f("env2_attack", "Env 2 Attack", 0.001, 10.0, 0.005, "s", true),
    f("env2_decay", "Env 2 Decay", 0.001, 10.0, 0.2, "s", true),
    f("env2_sustain", "Env 2 Sustain", 0.0, 1.0, 0.8, "", false),
    f("env2_release", "Env 2 Release", 0.001, 10.0, 0.3, "s", true),
    e("env2_shape", "Env 2 Shape", SHAPE_LABELS, 1.0),
    // Modulation matrix (source-major, dest-minor). ENV-2→Amp seeds to 1.0.
    mp("env1_pitch", "Env1→Pitch", 0.0),
    mc("env1_cutoff", "Env1→Cutoff"),
    ma("env1_amp", "Env1→Amp", 0.0),
    mw("env1_pwm", "Env1→PWM"),
    mp("env2_pitch", "Env2→Pitch", 0.0),
    mc("env2_cutoff", "Env2→Cutoff"),
    ma("env2_amp", "Env2→Amp", 1.0),
    mw("env2_pwm", "Env2→PWM"),
    // Gentle always-on vibrato by default (~5 cents at the 5 Hz LFO rate).
    mp("lfo_pitch", "LFO→Pitch", 0.05),
    mc("lfo_cutoff", "LFO→Cutoff"),
    ma("lfo_amp", "LFO→Amp", 0.0),
    mw("lfo_pwm", "LFO→PWM"),
    mp("vel_pitch", "Vel→Pitch", 0.0),
    mc("vel_cutoff", "Vel→Cutoff"),
    ma("vel_amp", "Vel→Amp", 0.0),
    mw("vel_pwm", "Vel→PWM"),
    mp("key_pitch", "Key→Pitch", 0.0),
    mc("key_cutoff", "Key→Cutoff"),
    ma("key_amp", "Key→Amp", 0.0),
    mw("key_pwm", "Key→PWM"),
    e("lfo_shape", "LFO Shape", LFO_LABELS, 0.0),
    f("lfo_rate", "LFO Rate", 0.01, 40.0, 5.0, "Hz", true),
    // ── Appended after v1 (E001); in-block offsets stay stable above this line. ──
    f("hpf_cutoff", "HPF Cutoff", 20.0, 18000.0, 20.0, "Hz", true),
    i("osc1_octave", "Osc 1 Octave", -4.0, 4.0, 0.0, "oct"),
    i("osc2_octave", "Osc 2 Octave", -4.0, 4.0, 0.0, "oct"),
    f("lfo_delay", "LFO Delay", 0.0, 4.0, 0.0, "s", false),
    // ── E002 (offsets stay stable above this line) ──
    b("osc_sync", "Sync", 0.0),
    f("cross_mod", "Cross Mod", 0.0, 1.0, 0.0, "", false),
    e("mod_wheel_dest", "Mod Wheel", MOD_WHEEL_DEST_LABELS, 0.0),
    f(
        "mod_wheel_depth",
        "Mod Wheel Depth",
        -48.0,
        48.0,
        12.0,
        "st",
        false,
    ),
    // ── E003 (offsets stay stable above this line) ──
    e("assign_mode", "Assign", ASSIGN_LABELS, 0.0),
];

/// Global descriptor table; indexed by [`GlobalParam`].
pub static GLOBAL_PARAMS: [ParamDesc; GlobalParam::COUNT] = [
    f("master_tune", "Master Tune", -12.0, 12.0, 0.0, "st", false),
    f("master_volume", "Volume", 0.0, 1.0, 0.7, "", false),
    b("chorus_on", "Chorus", 1.0),
    f("chorus_rate", "Chorus Rate", 0.05, 8.0, 0.6, "Hz", true),
    f("chorus_depth", "Chorus Depth", 0.0, 1.0, 0.5, "", false),
    f("chorus_mix", "Chorus Mix", 0.0, 1.0, 0.4, "", false),
    b("delay_on", "Delay", 0.0),
    f("delay_time", "Delay Time", 0.01, 2.0, 0.35, "s", true),
    f("delay_feedback", "Delay FB", 0.0, 0.95, 0.4, "", false),
    f("delay_mix", "Delay Mix", 0.0, 1.0, 0.25, "", false),
    b("delay_pingpong", "Ping-Pong", 1.0),
    e("oversample", "Oversample", OVERSAMPLE_LABELS, 1.0),
];

// ── Value storage ─────────────────────────────────────────────────────────────

#[inline]
fn enum_index(value: f32, max: usize) -> usize {
    (value.round() as usize).min(max)
}

/// One layer's worth of per-patch values (plain units). A **self-contained,
/// serializable unit** (ADR 0003 §6 / ticket 0007): a future single-patch preset
/// loads straight into one of these.
#[derive(Clone)]
pub struct PatchValues {
    v: [f32; PatchParam::COUNT],
}

impl Default for PatchValues {
    fn default() -> Self {
        let mut v = [0.0; PatchParam::COUNT];
        for (idx, d) in PATCH_PARAMS.iter().enumerate() {
            v[idx] = d.default;
        }
        Self { v }
    }
}

impl PatchValues {
    #[inline]
    pub fn get(&self, p: PatchParam) -> f32 {
        self.v[p.index()]
    }

    #[inline]
    pub fn get_index(&self, index: usize) -> f32 {
        self.v[index]
    }

    #[inline]
    pub fn set(&mut self, p: PatchParam, value: f32) {
        self.v[p.index()] = p.desc().clamp(value);
    }

    #[inline]
    pub fn set_index(&mut self, index: usize, value: f32) {
        if let Some(p) = PatchParam::from_index(index) {
            self.set(p, value);
        }
    }

    #[inline]
    pub fn bool(&self, p: PatchParam) -> bool {
        self.get(p) >= 0.5
    }

    pub fn osc_wave(&self, p: PatchParam) -> Waveform {
        Waveform::ALL[enum_index(self.get(p), Waveform::ALL.len() - 1)]
    }

    pub fn noise_color(&self) -> NoiseColor {
        NoiseColor::ALL[enum_index(self.get(PatchParam::NoiseColor), NoiseColor::ALL.len() - 1)]
    }

    pub fn filter_variant(&self) -> LadderVariant {
        if enum_index(self.get(PatchParam::FilterVariant), 1) == 0 {
            LadderVariant::Sharp
        } else {
            LadderVariant::Smooth
        }
    }

    pub fn lfo_shape(&self) -> LfoShape {
        LfoShape::ALL[enum_index(self.get(PatchParam::LfoShape), LfoShape::ALL.len() - 1)]
    }

    pub fn assign_mode(&self) -> AssignMode {
        AssignMode::from_index(enum_index(self.get(PatchParam::AssignMode), 1))
    }

    pub fn env1_shape(&self) -> AdsrShape {
        self.adsr_shape(PatchParam::Env1Shape)
    }

    pub fn env2_shape(&self) -> AdsrShape {
        self.adsr_shape(PatchParam::Env2Shape)
    }

    fn adsr_shape(&self, p: PatchParam) -> AdsrShape {
        if enum_index(self.get(p), 1) == 0 {
            AdsrShape::Linear
        } else {
            AdsrShape::Exponential
        }
    }
}

/// The global value block (master, FX, oversample).
#[derive(Clone)]
pub struct GlobalValues {
    v: [f32; GlobalParam::COUNT],
}

impl Default for GlobalValues {
    fn default() -> Self {
        let mut v = [0.0; GlobalParam::COUNT];
        for (idx, d) in GLOBAL_PARAMS.iter().enumerate() {
            v[idx] = d.default;
        }
        Self { v }
    }
}

impl GlobalValues {
    #[inline]
    pub fn get(&self, g: GlobalParam) -> f32 {
        self.v[g.index()]
    }

    #[inline]
    pub fn get_index(&self, index: usize) -> f32 {
        self.v[index]
    }

    #[inline]
    pub fn set(&mut self, g: GlobalParam, value: f32) {
        self.v[g.index()] = g.desc().clamp(value);
    }

    #[inline]
    pub fn set_index(&mut self, index: usize, value: f32) {
        if let Some(g) = GlobalParam::from_index(index) {
            self.set(g, value);
        }
    }

    #[inline]
    pub fn bool(&self, g: GlobalParam) -> bool {
        self.get(g) >= 0.5
    }

    /// Oversampling factor for the synthesis path: 1 (Off), 2 or 4.
    pub fn oversample_factor(&self) -> usize {
        match enum_index(self.get(GlobalParam::Oversample), 2) {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }
}

/// The complete engine-side value set: two per-patch layers plus the global
/// block. Addressed typed (per layer / global) by the engine, or by CLAP id at
/// the host/UI boundary via [`Self::get_by_clap_id`] / [`Self::set_by_clap_id`].
#[derive(Clone, Default)]
pub struct ParamValues {
    pub layers: [PatchValues; Layer::COUNT],
    pub global: GlobalValues,
}

impl ParamValues {
    #[inline]
    pub fn layer(&self, layer: Layer) -> &PatchValues {
        &self.layers[layer as usize]
    }

    #[inline]
    pub fn layer_mut(&mut self, layer: Layer) -> &mut PatchValues {
        &mut self.layers[layer as usize]
    }

    #[inline]
    pub fn global(&self) -> &GlobalValues {
        &self.global
    }

    #[inline]
    pub fn global_mut(&mut self) -> &mut GlobalValues {
        &mut self.global
    }

    /// Read a value by CLAP id (host/UI boundary).
    #[inline]
    pub fn get_by_clap_id(&self, clap_id: usize) -> f32 {
        match param_ref(clap_id) {
            Some(ParamRef::Patch(layer, p)) => self.layer(layer).get(p),
            Some(ParamRef::Global(g)) => self.global.get(g),
            None => 0.0,
        }
    }

    /// Write a value (clamped) by CLAP id (host/UI boundary). Out-of-range ids
    /// are ignored.
    #[inline]
    pub fn set_by_clap_id(&mut self, clap_id: usize, value: f32) {
        match param_ref(clap_id) {
            Some(ParamRef::Patch(layer, p)) => self.layer_mut(layer).set(p, value),
            Some(ParamRef::Global(g)) => self.global.set(g, value),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_len_match_counts() {
        assert_eq!(PATCH_PARAMS.len(), PatchParam::COUNT);
        assert_eq!(GLOBAL_PARAMS.len(), GlobalParam::COUNT);
    }

    #[test]
    fn from_index_roundtrips() {
        for p in PatchParam::all() {
            assert_eq!(PatchParam::from_index(p.index()), Some(p));
        }
        assert_eq!(PatchParam::from_index(PatchParam::COUNT), None);
        for g in GlobalParam::all() {
            assert_eq!(GlobalParam::from_index(g.index()), Some(g));
        }
        assert_eq!(GlobalParam::from_index(GlobalParam::COUNT), None);
    }

    #[test]
    fn clap_id_layout_is_contiguous_and_invertible() {
        // Upper block, then Lower block, then global block — no gaps, no overlap.
        let mut expected = 0usize;
        for layer in Layer::ALL {
            for p in PatchParam::all() {
                let id = patch_clap_id(layer, p);
                assert_eq!(id, expected, "{layer:?} {p:?} misindexed");
                assert_eq!(param_ref(id), Some(ParamRef::Patch(layer, p)));
                expected += 1;
            }
        }
        for g in GlobalParam::all() {
            let id = global_clap_id(g);
            assert_eq!(id, expected);
            assert_eq!(param_ref(id), Some(ParamRef::Global(g)));
            expected += 1;
        }
        assert_eq!(expected, TOTAL_PARAMS);
        assert_eq!(param_ref(TOTAL_PARAMS), None);
    }

    #[test]
    fn defaults_in_range() {
        let p = ParamValues::default();
        for id in 0..TOTAL_PARAMS {
            let d = desc_for_clap_id(id).unwrap();
            let val = p.get_by_clap_id(id);
            assert!(val >= d.min && val <= d.max, "{} default OOR", d.name);
        }
    }

    #[test]
    fn matrix_layout_is_contiguous_and_ordered() {
        // The 20 matrix params sit at MATRIX_BASE in source-major, dest-minor
        // order within the per-patch block so matrix_index() addresses them.
        assert_eq!(PatchParam::MATRIX_BASE, PatchParam::Env1Pitch.index());
        assert_eq!(
            PatchParam::matrix_index(ModSource::Env2, ModDest::Amp),
            PatchParam::Env2Amp.index()
        );
        assert_eq!(
            PatchParam::matrix_index(ModSource::KeyFollow, ModDest::Pwm),
            PatchParam::KeyPwm.index()
        );
        assert_eq!(
            PatchParam::matrix_index(ModSource::Lfo, ModDest::Cutoff),
            PatchParam::LfoCutoff.index()
        );
        assert!(PatchParam::is_matrix_param(PatchParam::Env1Pitch.index()));
        assert!(!PatchParam::is_matrix_param(PatchParam::LfoShape.index()));
        // ENV-2→Amp is the only route that defaults non-zero.
        assert_eq!(PatchValues::default().get(PatchParam::Env2Amp), 1.0);
    }

    #[test]
    fn clap_id_roundtrip_through_values() {
        // A value written by CLAP id lands in the right layer/global slot.
        let mut pv = ParamValues::default();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        let lo = patch_clap_id(Layer::Lower, PatchParam::Cutoff);
        pv.set_by_clap_id(up, 1000.0);
        pv.set_by_clap_id(lo, 2000.0);
        assert_eq!(pv.layer(Layer::Upper).get(PatchParam::Cutoff), 1000.0);
        assert_eq!(pv.layer(Layer::Lower).get(PatchParam::Cutoff), 2000.0);
        assert_eq!(pv.get_by_clap_id(up), 1000.0);
        // Clamping applies on the way in.
        let res = patch_clap_id(Layer::Upper, PatchParam::Resonance);
        pv.set_by_clap_id(res, 5.0);
        assert_eq!(pv.get_by_clap_id(res), 1.0);
    }

    #[test]
    fn key_mode_roundtrips() {
        for m in KeyMode::ALL {
            assert_eq!(KeyMode::from_u8(m as u8), m);
        }
        assert_eq!(KeyMode::default(), KeyMode::Whole);
    }
}
