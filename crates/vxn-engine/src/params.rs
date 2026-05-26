//! VXN1 parameter model — split into a per-patch block and a global block
//! (ADR 0003 §6).
//!
//! A **layer is a complete patch**: oscillators, filter, envelopes, LFO
//! and the fixed modulation routes. Those live in [`PatchParam`] and are
//! instantiated **twice** (Upper, Lower — see [`Layer`]), so each layer is
//! independently automatable. Truly global state (master tune/volume, FX,
//! oversample, the global LFO 2) lives once in [`GlobalParam`].
//!
//! ## Fixed modulation routes (E006 / 0022, ADR 0004)
//!
//! The old generic 6×4 modulation matrix is gone. Modulation is now a small set
//! of **fixed, labelled routes**, each carrying a per-channel source *selector*
//! plus a depth:
//!
//! - **Pitch** (common, vibrato-scaled — moves *both* oscillators): an LFO
//!   selector ({Off/LFO1/LFO2}) + depth, an Env selector ({Off/Env1/Env2}) +
//!   depth, and a pitch-wheel depth.
//! - **PWM**: LFO selector + depth, Env selector + depth.
//! - **Cutoff**: LFO selector + depth, Env selector + depth, velocity depth.
//! - **Osc 2 pitch** (wide, octave range — moves *osc2 only*, for sync/cross-mod
//!   sweeps): Env selector + depth, fed also by the mod-wheel panel.
//! - **Mod-wheel panel** (independent of the per-channel selectors): depths into
//!   PWM, cutoff, resonance and the wide osc2 pitch.
//! - **Filter key-track** (bool): exactly one octave of cutoff per octave of key
//!   relative to C4 (cutoff unchanged at C4, up above, down below).
//! - The **VCA is hardwired to Env2** — there is no Amp route.
//!
//! ## CLAP id layout
//!
//! Every automatable parameter needs an integer id. The id space is three
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
//! for the CLAP/UI metadata callbacks. There is no CLAP id-stability constraint
//! pre-release, so the table is laid out for clarity, not append-only.
//!
//! `KeyMode` and the split point are **not** in this table: they are
//! non-automatable shared state (ADR 0003 §3, §8), carried as atomics in
//! [`crate::SharedParams`] and persisted by [`crate::state`].
//!
//! Values are stored as `f32` in *plain* units (Hz, seconds, semitones, …),
//! matching CLAP's plain-value convention. Enum/bool params store the variant
//! index / 0.0|1.0 and are read back through typed accessors.

use vxn_dsp::{AdsrShape, LadderVariant, LfoShape, Waveform};

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
    /// Monophonic: only one channel sounds per layer; a new note takes over the
    /// sounding channel (last-note priority), so glide is legato.
    Solo,
    /// Each note is assigned to **two** channels with a pitch spread and phase
    /// decorrelation — a fat 2-voice-per-note stack. Halves effective polyphony
    /// (the pool is shared 2:1). Named to avoid clashing with the keyboard-level
    /// `KeyMode::Dual` (note → both layers).
    Twin,
}

impl AssignMode {
    /// Number of assign modes; the enum param spans indices `0..COUNT`.
    pub const COUNT: usize = AssignMode::Twin as usize + 1;

    pub fn from_index(i: usize) -> AssignMode {
        match i {
            1 => AssignMode::Unison,
            2 => AssignMode::Solo,
            3 => AssignMode::Twin,
            _ => AssignMode::Poly,
        }
    }
}

/// Per-channel **LFO source** selector for a fixed modulation route (ADR 0004
/// §4). Either LFO can feed any channel; there are no dedicated LFO-2 routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum LfoSel {
    #[default]
    Off,
    Lfo1,
    Lfo2,
}

impl LfoSel {
    pub const COUNT: usize = 3;
    pub const ALL: [LfoSel; Self::COUNT] = [LfoSel::Off, LfoSel::Lfo1, LfoSel::Lfo2];

    pub fn from_index(i: usize) -> LfoSel {
        match i {
            1 => LfoSel::Lfo1,
            2 => LfoSel::Lfo2,
            _ => LfoSel::Off,
        }
    }
}

/// Per-channel **envelope source** selector for a fixed modulation route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum EnvSel {
    #[default]
    Off,
    Env1,
    Env2,
}

impl EnvSel {
    pub const COUNT: usize = 3;
    pub const ALL: [EnvSel; Self::COUNT] = [EnvSel::Off, EnvSel::Env1, EnvSel::Env2];

    pub fn from_index(i: usize) -> EnvSel {
        match i {
            1 => EnvSel::Env1,
            2 => EnvSel::Env2,
            _ => EnvSel::Off,
        }
    }
}

/// Oscillator-interaction type (ADR 0004 §3): the three modes are mutually
/// exclusive. `Off` is the independent, vectorised fast path (bit-identical),
/// `Sync` drives the band-limited hard sync (0020), `Pm` drives the through-zero
/// phase modulation (0022) at [`PatchParam::CrossModAmount`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum CrossModType {
    #[default]
    Off,
    Sync,
    Pm,
}

impl CrossModType {
    pub const COUNT: usize = 3;
    pub const ALL: [CrossModType; Self::COUNT] =
        [CrossModType::Off, CrossModType::Sync, CrossModType::Pm];

    pub fn from_index(i: usize) -> CrossModType {
        match i {
            1 => CrossModType::Sync,
            2 => CrossModType::Pm,
            _ => CrossModType::Off,
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
    Osc1Octave,
    Osc1Level,
    Osc1PulseWidth,
    // Oscillator 2
    Osc2Wave,
    Osc2Coarse,
    Osc2Fine,
    Osc2Octave,
    Osc2Level,
    Osc2PulseWidth,
    // Oscillator interaction (type selector + amount; ADR 0004 §3/§7)
    CrossModType,
    CrossModAmount,
    // Mixer
    RingLevel,
    // Filter (ladder VCF)
    Cutoff,
    Resonance,
    Drive,
    FilterVariant,
    /// Pre-VCF high-pass cutoff (Hz). 20 ≈ fully open / "off".
    HpfCutoff,
    /// Key-track on/off: cutoff shifts exactly 1 octave per key octave relative
    /// to C4 (unchanged at C4, up above it, down below it).
    FilterKeyTrack,
    // Envelope 1 (assignable via the route selectors; defaults unrouted)
    Env1Attack,
    Env1Decay,
    Env1Sustain,
    Env1Release,
    Env1Shape,
    // Envelope 2 (hardwired to the VCA amp)
    Env2Attack,
    Env2Decay,
    Env2Sustain,
    Env2Release,
    Env2Shape,
    // LFO 1 (per-voice — E005 / 0018). LFO 2's shape/rate/sync are global.
    LfoShape,
    LfoRate,
    LfoSync,
    /// LFO 1 per-voice onset (E005 / 0018): hold modulation at zero this long
    /// after note-on, then ramp over `Lfo1Fade`.
    Lfo1DelayTime,
    /// LFO 1 per-voice fade-ramp duration (s) after the delay; 0 = snap to full.
    Lfo1Fade,
    /// LFO 1 free-run: when on, the per-voice phase persists across note-ons.
    Lfo1FreeRun,
    // ── Pitch route (common, vibrato-scaled — both oscillators) ──
    PitchLfoSrc,
    PitchLfoDepth,
    PitchEnvSrc,
    PitchEnvDepth,
    /// Pitch-wheel (MIDI bend) range, in vibrato-scaled semitones.
    PitchWheelDepth,
    // ── PWM route ──
    PwmLfoSrc,
    PwmLfoDepth,
    PwmEnvSrc,
    PwmEnvDepth,
    // ── Cutoff route ── fixed sources (E006): velocity, both LFOs and Env 1 each
    // get their own depth into cutoff (no source selectors). Env→cutoff is always
    // Env 1; Env 2 is the VCA env.
    CutoffLfo1Depth,
    CutoffLfo2Depth,
    CutoffEnvDepth,
    VelCutoffDepth,
    // ── Wide osc-2 pitch route (sync-sweep — osc2 only, octave range) ──
    Osc2PitchEnvSrc,
    Osc2PitchEnvDepth,
    // ── Mod-wheel panel (independent of the per-channel selectors) ──
    ModWheelPwm,
    ModWheelCutoff,
    ModWheelReso,
    ModWheelOsc2Pitch,
    // ── Voice assignment / glide (E003) ── glide has no on/off: time 0 = off.
    AssignMode,
    UnisonDetune,
    PortamentoTime,
}

impl PatchParam {
    pub const COUNT: usize = PatchParam::PortamentoTime as usize + 1;

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
    DelaySync,
    // Quality
    Oversample,
    // Global LFO 2 (E005 / 0019): a single instrument-wide LFO. It reaches the
    // routes through the per-channel {Off/LFO1/LFO2} selectors; shape/rate/sync
    // are global.
    Lfo2Shape,
    Lfo2Rate,
    Lfo2Sync,
}

impl GlobalParam {
    pub const COUNT: usize = GlobalParam::Lfo2Sync as usize + 1;

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
    Float { unit: &'static str, taper: Taper },
    Int { unit: &'static str },
    Bool,
    Enum { variants: &'static [&'static str] },
}

/// How a continuous (Float) param maps across a fader's normalized `[0, 1]`
/// position — the single declarative source for a control's feel, replacing the
/// old per-control mapping. Read by the editor's fader (`to_fader`/`from_fader`);
/// `to_normalized`/`from_normalized` stay **linear** (they back the CLAP/host
/// value normalization and the synced-LFO subdivision index, which must not warp).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taper {
    /// Straight `min → max`.
    Linear,
    /// Exponential, pinned so the fader **midpoint reads `mid`** and the **top
    /// reads `max`** (bottom = 0): `v = A·(e^(K·n) − 1)`. For a subtle low end
    /// (envelope times, LFO rates, filter cutoffs) without cramming it into the
    /// bottom of the travel.
    Exp { mid: f32 },
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

    /// This param's [`Taper`] (Linear for non-Float kinds).
    #[inline]
    pub fn taper(&self) -> Taper {
        match self.kind {
            ParamKind::Float { taper, .. } => taper,
            _ => Taper::Linear,
        }
    }

    /// Exp-taper coefficients `(A, K)` for `v = A·(e^(K·n) − 1)`, or `None` when
    /// linear. Pinned through the midpoint (`mid`) and top (`max`); bottom = 0.
    #[inline]
    fn exp_coeffs(&self) -> Option<(f32, f32)> {
        match self.taper() {
            Taper::Exp { mid } => {
                let r = self.max / mid - 1.0; // = e^(K/2)
                Some((mid / (r - 1.0), 2.0 * r.ln()))
            }
            Taper::Linear => None,
        }
    }

    /// Value → fader position `[0, 1]`, applying the [`Taper`]. The editor's fader
    /// mapping; for a linear param this is exactly [`to_normalized`].
    ///
    /// [`to_normalized`]: ParamDesc::to_normalized
    #[inline]
    pub fn to_fader(&self, value: f32) -> f32 {
        match self.exp_coeffs() {
            Some((a, k)) => ((value / a + 1.0).ln() / k).clamp(0.0, 1.0),
            None => self.to_normalized(value),
        }
    }

    /// Fader position `[0, 1]` → value, applying the [`Taper`] (inverse of
    /// [`to_fader`]).
    ///
    /// [`to_fader`]: ParamDesc::to_fader
    #[inline]
    pub fn from_fader(&self, n: f32) -> f32 {
        match self.exp_coeffs() {
            Some((a, k)) => a * ((k * n.clamp(0.0, 1.0)).exp() - 1.0),
            None => self.from_normalized(n),
        }
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
const VARIANT_LABELS: &[&str] = &["Sharp", "Smooth"];
const SHAPE_LABELS: &[&str] = &["Lin", "Exp"];
const LFO_LABELS: &[&str] = &["Sine", "Tri", "Saw+", "Saw-", "Square", "S&H"];
const OVERSAMPLE_LABELS: &[&str] = &["Off", "2x", "4x", "8x"];
const ASSIGN_LABELS: &[&str] = &["Poly", "Unison", "Solo", "Twin"];
const LFO_SEL_LABELS: &[&str] = &["Off", "LFO 1", "LFO 2"];
const ENV_SEL_LABELS: &[&str] = &["Off", "Env 1", "Env 2"];
/// PM is labelled "FM" in the table — players expect that name (ADR 0004 §3).
const CROSS_MOD_LABELS: &[&str] = &["Off", "Sync", "FM"];

const fn f(
    name: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: &'static str,
    taper: Taper,
) -> ParamDesc {
    ParamDesc {
        name,
        label,
        min,
        max,
        default,
        kind: ParamKind::Float { unit, taper },
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
/// Vibrato-scaled pitch depth (semitones) for the common pitch channel: narrow
/// range so the knob feel suits vibrato, not sweeps. `default` seeds the gentle
/// always-on vibrato.
const fn mp_vib(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(name, label, -12.0, 12.0, default, "st", Taper::Linear)
}
/// LFO→pitch vibrato depth: unipolar 0..12 st (the LFO is already bipolar, so a
/// negative depth would only flip phase). The UI tapers it so the midpoint reads
/// 1 st — vibrato is mostly meant to be very subtle.
const fn mp_vib_lfo(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(name, label, 0.0, 12.0, default, "st", Taper::Exp { mid: 1.0 })
}
/// Wide osc-2 pitch depth (semitones): octave range, for sync/cross-mod sweeps.
const fn mp_wide(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -48.0, 48.0, 0.0, "st", Taper::Linear)
}
/// Cutoff-destination depth (semitones of cutoff). Bipolar — env / velocity into
/// cutoff can sweep either way.
const fn mc(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -96.0, 96.0, 0.0, "st", Taper::Linear)
}
/// Unipolar LFO→cutoff depth (0..96 st): an LFO is symmetric, so only the
/// positive half is exposed (a negative depth would just flip an inaudible
/// phase). Keeps the stored/automation range identical to the fader's.
const fn mcu(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, 0.0, 96.0, 0.0, "st", Taper::Linear)
}
/// PWM-destination depth (pulse-width fraction). Bipolar (env / mod-wheel).
const fn mw(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -0.5, 0.5, 0.0, "", Taper::Linear)
}
/// Unipolar LFO→PWM depth (0..0.5): symmetric LFO, positive half only.
const fn mwu(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, 0.0, 0.5, 0.0, "", Taper::Linear)
}
/// LFO source selector.
const fn lfosel(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    e(name, label, LFO_SEL_LABELS, default)
}
/// Env source selector.
const fn envsel(name: &'static str, label: &'static str) -> ParamDesc {
    e(name, label, ENV_SEL_LABELS, 0.0)
}

/// Per-patch descriptor table; indexed by [`PatchParam`] (= in-block offset).
pub static PATCH_PARAMS: [ParamDesc; PatchParam::COUNT] = [
    // Oscillator 1
    e("osc1_wave", "Osc 1 Wave", WAVE_LABELS, 2.0),
    i("osc1_coarse", "Osc 1 Coarse", -24.0, 24.0, 0.0, "st"),
    f("osc1_fine", "Osc 1 Fine", -50.0, 50.0, 0.0, "ct", Taper::Linear),
    i("osc1_octave", "Osc 1 Octave", -4.0, 4.0, 0.0, "oct"),
    f("osc1_level", "Osc 1 Level", 0.0, 1.0, 0.8, "", Taper::Linear),
    f("osc1_pw", "Osc 1 PW", 0.05, 0.95, 0.5, "", Taper::Linear),
    // Oscillator 2
    e("osc2_wave", "Osc 2 Wave", WAVE_LABELS, 2.0),
    i("osc2_coarse", "Osc 2 Coarse", -24.0, 24.0, -12.0, "st"),
    f("osc2_fine", "Osc 2 Fine", -50.0, 50.0, 7.0, "ct", Taper::Linear),
    i("osc2_octave", "Osc 2 Octave", -4.0, 4.0, 0.0, "oct"),
    f("osc2_level", "Osc 2 Level", 0.0, 1.0, 0.6, "", Taper::Linear),
    f("osc2_pw", "Osc 2 PW", 0.05, 0.95, 0.5, "", Taper::Linear),
    // Oscillator interaction
    e("cross_mod_type", "Cross Mod", CROSS_MOD_LABELS, 0.0),
    f("cross_mod_amount", "Cross Mod Amt", 0.0, 4.0, 0.0, "", Taper::Linear),
    // Mixer
    f("ring_level", "Ring Level", 0.0, 1.0, 0.0, "", Taper::Linear),
    // Filter
    f("cutoff", "Cutoff", 20.0, 18000.0, 8000.0, "Hz", Taper::Exp { mid: 1000.0 }),
    f("resonance", "Resonance", 0.0, 1.0, 0.2, "", Taper::Linear),
    f("drive", "Drive", 0.1, 4.0, 1.0, "", Taper::Linear),
    e("filter_variant", "Filter Type", VARIANT_LABELS, 0.0),
    f("hpf_cutoff", "HPF Cutoff", 20.0, 18000.0, 20.0, "Hz", Taper::Exp { mid: 1000.0 }),
    b("filter_key_track", "Key Track", 0.0),
    // Envelope 1
    f("env1_attack", "Env 1 Attack", 0.001, 10.0, 0.005, "s", Taper::Exp { mid: 1.0 }),
    f("env1_decay", "Env 1 Decay", 0.001, 10.0, 0.3, "s", Taper::Exp { mid: 1.0 }),
    f("env1_sustain", "Env 1 Sustain", 0.0, 1.0, 0.0, "", Taper::Linear),
    f("env1_release", "Env 1 Release", 0.001, 10.0, 0.3, "s", Taper::Exp { mid: 1.0 }),
    e("env1_shape", "Env 1 Shape", SHAPE_LABELS, 0.0),
    // Envelope 2 (VCA)
    f("env2_attack", "Env 2 Attack", 0.001, 10.0, 0.005, "s", Taper::Exp { mid: 1.0 }),
    f("env2_decay", "Env 2 Decay", 0.001, 10.0, 0.2, "s", Taper::Exp { mid: 1.0 }),
    f("env2_sustain", "Env 2 Sustain", 0.0, 1.0, 0.8, "", Taper::Linear),
    f("env2_release", "Env 2 Release", 0.001, 10.0, 0.3, "s", Taper::Exp { mid: 1.0 }),
    e("env2_shape", "Env 2 Shape", SHAPE_LABELS, 1.0),
    // LFO 1
    e("lfo_shape", "LFO 1 Shape", LFO_LABELS, 0.0),
    f("lfo_rate", "LFO 1 Rate", 0.01, 40.0, 5.0, "Hz", Taper::Exp { mid: 5.0 }),
    b("lfo_sync", "LFO 1 Sync", 0.0),
    f("lfo1_delay_time", "LFO 1 Delay", 0.0, 4.0, 0.0, "s", Taper::Linear),
    f("lfo1_fade", "LFO 1 Fade", 0.0, 4.0, 0.0, "s", Taper::Linear),
    b("lfo1_free_run", "LFO 1 Free", 0.0),
    // Pitch route (common, vibrato-scaled). Default vibrato: LFO 1 → pitch at a
    // gentle depth, so the default patch sounds as it did with the old matrix.
    lfosel("pitch_lfo_src", "Pitch LFO", 1.0),
    mp_vib_lfo("pitch_lfo_depth", "Pitch LFO Dep", 0.05),
    envsel("pitch_env_src", "Pitch Env"),
    mp_vib("pitch_env_depth", "Pitch Env Dep", 0.0),
    f("pitch_wheel_depth", "Pitch Wheel", 0.0, 12.0, 2.0, "st", Taper::Linear),
    // PWM route
    lfosel("pwm_lfo_src", "PWM LFO", 0.0),
    mwu("pwm_lfo_depth", "PWM LFO Dep"),
    envsel("pwm_env_src", "PWM Env"),
    mw("pwm_env_depth", "PWM Env Dep"),
    // Cutoff route — fixed sources (E006), one depth each. LFO depths are unipolar
    // (0..max); env / velocity stay bipolar.
    mcu("cutoff_lfo1_depth", "Cutoff LFO1 Dep"),
    mcu("cutoff_lfo2_depth", "Cutoff LFO2 Dep"),
    mc("cutoff_env_depth", "Cutoff Env Dep"),
    mc("vel_cutoff_depth", "Vel→Cutoff"),
    // Wide osc-2 pitch route
    envsel("osc2_pitch_env_src", "Osc2 Pitch Env"),
    mp_wide("osc2_pitch_env_depth", "Osc2 Pitch Dep"),
    // Mod-wheel panel
    mw("mod_wheel_pwm", "Wheel→PWM"),
    mc("mod_wheel_cutoff", "Wheel→Cutoff"),
    f("mod_wheel_reso", "Wheel→Reso", 0.0, 1.0, 0.0, "", Taper::Linear),
    mp_wide("mod_wheel_osc2_pitch", "Wheel→Osc2"),
    // Voice assignment / glide
    e("assign_mode", "Assign", ASSIGN_LABELS, 0.0),
    f("unison_detune", "Detune", 0.0, 50.0, 12.0, "ct", Taper::Linear),
    f("portamento_time", "Glide Time", 0.0, 0.5, 0.0, "s", Taper::Exp { mid: 0.1 }),
];

/// Global descriptor table; indexed by [`GlobalParam`].
pub static GLOBAL_PARAMS: [ParamDesc; GlobalParam::COUNT] = [
    f("master_tune", "Master Tune", -12.0, 12.0, 0.0, "st", Taper::Linear),
    f("master_volume", "Volume", 0.0, 1.0, 0.7, "", Taper::Linear),
    b("chorus_on", "Chorus", 1.0),
    f("chorus_rate", "Chorus Rate", 0.05, 8.0, 0.6, "Hz", Taper::Linear),
    f("chorus_depth", "Chorus Depth", 0.0, 1.0, 0.5, "", Taper::Linear),
    f("chorus_mix", "Chorus Mix", 0.0, 1.0, 0.4, "", Taper::Linear),
    b("delay_on", "Delay", 0.0),
    f("delay_time", "Delay Time", 0.01, 2.0, 0.35, "s", Taper::Linear),
    f("delay_feedback", "Delay FB", 0.0, 0.95, 0.4, "", Taper::Linear),
    f("delay_mix", "Delay Mix", 0.0, 1.0, 0.25, "", Taper::Linear),
    b("delay_pingpong", "Ping-Pong", 1.0),
    b("delay_sync", "Delay Sync", 0.0),
    e("oversample", "Oversample", OVERSAMPLE_LABELS, 1.0),
    // Global LFO 2 (E005 / 0019).
    e("lfo2_shape", "LFO 2 Shape", LFO_LABELS, 0.0),
    f("lfo2_rate", "LFO 2 Rate", 0.01, 40.0, 5.0, "Hz", Taper::Exp { mid: 5.0 }),
    b("lfo2_sync", "LFO 2 Sync", 0.0),
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
        AssignMode::from_index(enum_index(self.get(PatchParam::AssignMode), AssignMode::COUNT - 1))
    }

    /// Read a per-channel LFO source selector.
    pub fn lfo_sel(&self, p: PatchParam) -> LfoSel {
        LfoSel::from_index(enum_index(self.get(p), LfoSel::COUNT - 1))
    }

    /// Read a per-channel envelope source selector.
    pub fn env_sel(&self, p: PatchParam) -> EnvSel {
        EnvSel::from_index(enum_index(self.get(p), EnvSel::COUNT - 1))
    }

    /// Read the oscillator-interaction type (Off / Sync / PM).
    pub fn cross_mod_type(&self) -> CrossModType {
        CrossModType::from_index(enum_index(
            self.get(PatchParam::CrossModType),
            CrossModType::COUNT - 1,
        ))
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

    /// Oversampling factor for the synthesis path: 1 (Off), 2, 4 or 8.
    pub fn oversample_factor(&self) -> usize {
        match enum_index(self.get(GlobalParam::Oversample), 3) {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        }
    }

    /// Global LFO 2 shape (E005 / 0019).
    pub fn lfo2_shape(&self) -> LfoShape {
        LfoShape::ALL[enum_index(self.get(GlobalParam::Lfo2Shape), LfoShape::ALL.len() - 1)]
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
    fn default_patch_keeps_gentle_vibrato() {
        // The default patch routes LFO 1 → pitch at a gentle depth, so it sounds
        // as it did under the old matrix.
        let p = PatchValues::default();
        assert_eq!(p.lfo_sel(PatchParam::PitchLfoSrc), LfoSel::Lfo1);
        assert_eq!(p.get(PatchParam::PitchLfoDepth), 0.05);
        // VCA is hardwired to Env2 (no Amp route to default non-zero).
        assert_eq!(p.env_sel(PatchParam::PitchEnvSrc), EnvSel::Off);
    }

    #[test]
    fn route_selectors_roundtrip() {
        let mut p = PatchValues::default();
        p.set(PatchParam::PwmLfoSrc, 2.0);
        assert_eq!(p.lfo_sel(PatchParam::PwmLfoSrc), LfoSel::Lfo2);
        p.set(PatchParam::PwmEnvSrc, 1.0);
        assert_eq!(p.env_sel(PatchParam::PwmEnvSrc), EnvSel::Env1);
        for (idx, t) in [
            (0.0, CrossModType::Off),
            (1.0, CrossModType::Sync),
            (2.0, CrossModType::Pm),
        ] {
            p.set(PatchParam::CrossModType, idx);
            assert_eq!(p.cross_mod_type(), t);
        }
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
