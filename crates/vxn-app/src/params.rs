//! Parameter model (ADR 0007 §4): the typed param ids, ranges, formatting and
//! CLAP-id ↔ typed lookup. Lives in `vxn-app` (not the engine) so a view crate
//! can read the descriptor table without depending on the engine's voice/render
//! code. The engine's storage types (`PatchValues` / `GlobalValues` /
//! `ParamValues`) index into these tables.
//!
//! ## Fixed modulation routes (E006 / 0022, ADR 0004)
//!
//! Modulation is a small set of **fixed, labelled routes**, each carrying a
//! per-channel source *selector* plus a depth (Pitch / PWM / Cutoff / Osc 2
//! pitch / mod-wheel / VCA — see the engine for behaviour).
//!
//! ## CLAP id layout
//!
//! ```text
//! [ 0 .. PATCH_COUNT )                 Upper per-patch params
//! [ PATCH_COUNT .. 2*PATCH_COUNT )     Lower per-patch params
//! [ 2*PATCH_COUNT .. TOTAL_PARAMS )    global params
//! ```
//!
//! [`patch_clap_id`] / [`global_clap_id`] map a typed param to its id;
//! [`param_ref`] / [`desc_for_clap_id`] invert it.

use crate::domain::Layer;

// ── Param-value enums (variant indices stored in the param array as f32) ────

/// Per-layer voice-assignment mode (ADR 0003 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum AssignMode {
    #[default]
    Poly,
    Unison,
    Solo,
    Twin,
}

impl AssignMode {
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

/// Per-channel **LFO source** selector for a fixed modulation route.
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

/// Oscillator-interaction type (ADR 0004 §3): Off / Sync / PM ("FM" in labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(usize)]
pub enum CrossModType {
    #[default]
    Off,
    Sync,
    Pm,
    Ring,
}

impl CrossModType {
    pub const COUNT: usize = 4;
    pub const ALL: [CrossModType; Self::COUNT] = [
        CrossModType::Off,
        CrossModType::Sync,
        CrossModType::Pm,
        CrossModType::Ring,
    ];

    pub fn from_index(i: usize) -> CrossModType {
        match i {
            1 => CrossModType::Sync,
            2 => CrossModType::Pm,
            3 => CrossModType::Ring,
            _ => CrossModType::Off,
        }
    }
}

// ── Param id enums ──────────────────────────────────────────────────────────

/// Per-patch parameter ids. Discriminant = index into the per-patch block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum PatchParam {
    Osc1Wave,
    Osc1Coarse,
    Osc1Fine,
    Osc1Octave,
    Osc1Level,
    Osc1PulseWidth,
    Osc2Wave,
    Osc2Coarse,
    Osc2Fine,
    Osc2Octave,
    Osc2Level,
    Osc2PulseWidth,
    SubLevel,
    CrossModType,
    CrossModAmount,
    NoiseLevel,
    NoiseColor,
    Cutoff,
    Resonance,
    Drive,
    FilterMode,
    FilterSlope,
    HpfCutoff,
    FilterKeyTrack,
    Env1Attack,
    Env1Decay,
    Env1Sustain,
    Env1Release,
    Env1Shape,
    Env2Attack,
    Env2Decay,
    Env2Sustain,
    Env2Release,
    Env2Shape,
    AmpLfoSrc,
    AmpLfoDepth,
    AmpEnvBypass,
    LfoShape,
    LfoRate,
    LfoSync,
    Lfo1DelayTime,
    Lfo1Fade,
    Lfo1FreeRun,
    PitchLfoSrc,
    PitchLfoDepth,
    PitchLfoModOnly,
    PitchEnvSrc,
    PitchEnvDepth,
    PitchEnvModOnly,
    PitchWheelDepth,
    PwmLfoSrc,
    PwmLfoDepth,
    PwmEnvSrc,
    PwmEnvDepth,
    CutoffLfo1Depth,
    CutoffLfo2Depth,
    CutoffEnvDepth,
    VelCutoffDepth,
    ModWheelPwm,
    ModWheelCutoff,
    ModWheelReso,
    ModWheelCrossModSweep,
    AssignMode,
    Legato,
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

    /// Resolve a [`ParamDesc::name`] string to its param (preset key lookup).
    pub fn from_name(name: &str) -> Option<PatchParam> {
        PATCH_PARAMS
            .iter()
            .position(|d| d.name == name)
            .and_then(Self::from_index)
    }

    pub fn desc(self) -> &'static ParamDesc {
        &PATCH_PARAMS[self.index()]
    }
}

/// Global parameter ids. Discriminant = index into the global block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum GlobalParam {
    MasterTune,
    MasterVolume,
    ChorusOn,
    ChorusRate,
    ChorusDepth,
    ChorusMix,
    DelayOn,
    DelayTime,
    DelayFeedback,
    DelayMix,
    DelayPingPong,
    DelaySync,
    ReverbOn,
    ReverbType,
    ReverbDepth,
    ReverbMix,
    LimiterOn,
    Oversample,
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

    pub fn from_name(name: &str) -> Option<GlobalParam> {
        GLOBAL_PARAMS
            .iter()
            .position(|d| d.name == name)
            .and_then(Self::from_index)
    }

    pub fn desc(self) -> &'static ParamDesc {
        &GLOBAL_PARAMS[self.index()]
    }
}

// ── CLAP id layout ──────────────────────────────────────────────────────────

pub const PATCH_COUNT: usize = PatchParam::COUNT;
pub const GLOBAL_COUNT: usize = GlobalParam::COUNT;
pub const TOTAL_PARAMS: usize = Layer::COUNT * PATCH_COUNT + GLOBAL_COUNT;

#[inline]
pub const fn patch_clap_id(layer: Layer, p: PatchParam) -> usize {
    (layer as usize) * PATCH_COUNT + (p as usize)
}

#[inline]
pub const fn global_clap_id(g: GlobalParam) -> usize {
    Layer::COUNT * PATCH_COUNT + (g as usize)
}

/// A resolved CLAP id: per-patch param on a layer, or global.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamRef {
    Patch(Layer, PatchParam),
    Global(GlobalParam),
}

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

pub fn desc_for_clap_id(clap_id: usize) -> Option<&'static ParamDesc> {
    match param_ref(clap_id)? {
        ParamRef::Patch(_, p) => Some(p.desc()),
        ParamRef::Global(g) => Some(g.desc()),
    }
}

pub fn module_for_clap_id(clap_id: usize) -> &'static str {
    match param_ref(clap_id) {
        Some(ParamRef::Patch(Layer::Upper, _)) => "Upper",
        Some(ParamRef::Patch(Layer::Lower, _)) => "Lower",
        Some(ParamRef::Global(_)) => "Global",
        None => "",
    }
}

// ── ParamDesc ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    Float { unit: &'static str, taper: Taper },
    Int { unit: &'static str },
    Bool,
    Enum { variants: &'static [&'static str] },
}

/// How a Float param maps across a fader's normalized `[0, 1]` position.
/// `to_normalized` / `from_normalized` stay linear (host range + synced-LFO
/// subdivision index must not warp).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taper {
    Linear,
    /// Pinned so midpoint reads `mid` and top reads `max`.
    Exp { mid: f32 },
}

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

    /// Resolve an enum **variant label** to its index (case-insensitive).
    pub fn variant_index(&self, label: &str) -> Option<usize> {
        match self.kind {
            ParamKind::Enum { variants } => {
                variants.iter().position(|v| v.eq_ignore_ascii_case(label))
            }
            _ => None,
        }
    }

    #[inline]
    pub fn taper(&self) -> Taper {
        match self.kind {
            ParamKind::Float { taper, .. } => taper,
            _ => Taper::Linear,
        }
    }

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

    #[inline]
    pub fn to_fader(&self, value: f32) -> f32 {
        match self.exp_coeffs() {
            Some((a, k)) => ((value / a + 1.0).ln() / k).clamp(0.0, 1.0),
            None => self.to_normalized(value),
        }
    }

    #[inline]
    pub fn from_fader(&self, n: f32) -> f32 {
        match self.exp_coeffs() {
            Some((a, k)) => a * ((k * n.clamp(0.0, 1.0)).exp() - 1.0),
            None => self.from_normalized(n),
        }
    }

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

// ── Descriptor tables ───────────────────────────────────────────────────────

const WAVE_LABELS: &[&str] = &["Sine", "Triangle", "Saw", "Pulse"];
const FILTER_MODE_LABELS: &[&str] = &["LP", "HP", "BP", "Notch"];
const SLOPE_LABELS: &[&str] = &["12", "24"];
const NOISE_LABELS: &[&str] = &["White", "Pink"];
const SHAPE_LABELS: &[&str] = &["Lin", "Exp"];
const LFO_LABELS: &[&str] = &["Sine", "Tri", "Saw+", "Saw-", "Square", "S&H"];
const OVERSAMPLE_LABELS: &[&str] = &["O/S OFF", "2x", "4x", "8x"];
pub const REVERB_TYPE_LABELS: &[&str] = &["Plate", "Room", "Hall", "Large"];
const ASSIGN_LABELS: &[&str] = &["Poly", "Unison", "Solo", "Twin"];
const LFO_SEL_LABELS: &[&str] = &["Off", "LFO 1", "LFO 2"];
const ENV_SEL_LABELS: &[&str] = &["Off", "Env 1", "Env 2"];
/// PM is labelled "FM" in the table — players expect that name (ADR 0004 §3).
const CROSS_MOD_LABELS: &[&str] = &["Off", "Sync", "FM", "Ring"];

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
const fn mp_vib(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(name, label, -12.0, 12.0, default, "st", Taper::Linear)
}
const fn mp_vib_lfo(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(
        name,
        label,
        0.0,
        12.0,
        default,
        "st",
        Taper::Exp { mid: 1.0 },
    )
}
const fn mp_wide(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -48.0, 48.0, 0.0, "st", Taper::Linear)
}
const fn mc(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -96.0, 96.0, 0.0, "st", Taper::Linear)
}
const fn mcu(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, 0.0, 96.0, 0.0, "st", Taper::Linear)
}
const fn mw(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, -0.5, 0.5, 0.0, "", Taper::Linear)
}
const fn mwu(name: &'static str, label: &'static str) -> ParamDesc {
    f(name, label, 0.0, 0.5, 0.0, "", Taper::Linear)
}
const fn lfosel(name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    e(name, label, LFO_SEL_LABELS, default)
}
const fn envsel(name: &'static str, label: &'static str) -> ParamDesc {
    e(name, label, ENV_SEL_LABELS, 0.0)
}

pub static PATCH_PARAMS: [ParamDesc; PatchParam::COUNT] = [
    e("osc1_wave", "Osc 1 Wave", WAVE_LABELS, 2.0),
    i("osc1_coarse", "Osc 1 Coarse", -7.0, 7.0, 0.0, "st"),
    f(
        "osc1_fine",
        "Osc 1 Fine",
        -50.0,
        50.0,
        0.0,
        "ct",
        Taper::Linear,
    ),
    i("osc1_octave", "Osc 1 Octave", -4.0, 4.0, 0.0, "oct"),
    f(
        "osc1_level",
        "Osc 1 Level",
        0.0,
        1.0,
        0.8,
        "",
        Taper::Linear,
    ),
    f("osc1_pw", "Osc 1 PW", 0.05, 0.95, 0.5, "", Taper::Linear),
    e("osc2_wave", "Osc 2 Wave", WAVE_LABELS, 2.0),
    i("osc2_coarse", "Osc 2 Coarse", -7.0, 7.0, 0.0, "st"),
    f(
        "osc2_fine",
        "Osc 2 Fine",
        -50.0,
        50.0,
        0.0,
        "ct",
        Taper::Linear,
    ),
    i("osc2_octave", "Osc 2 Octave", -4.0, 4.0, -1.0, "oct"),
    f(
        "osc2_level",
        "Osc 2 Level",
        0.0,
        1.0,
        0.6,
        "",
        Taper::Linear,
    ),
    f("osc2_pw", "Osc 2 PW", 0.05, 0.95, 0.5, "", Taper::Linear),
    f("sub_level", "Sub Level", 0.0, 1.0, 0.0, "", Taper::Linear),
    e("cross_mod_type", "Cross Mod", CROSS_MOD_LABELS, 0.0),
    f(
        "cross_mod_amount",
        "Cross Mod Amt",
        0.0,
        4.0,
        0.0,
        "",
        Taper::Linear,
    ),
    f(
        "noise_level",
        "Noise Level",
        0.0,
        1.0,
        0.0,
        "",
        Taper::Linear,
    ),
    e("noise_color", "Noise Colour", NOISE_LABELS, 0.0),
    f(
        "cutoff",
        "Cutoff",
        20.0,
        18000.0,
        8000.0,
        "Hz",
        Taper::Exp { mid: 1000.0 },
    ),
    f("resonance", "Resonance", 0.0, 1.0, 0.2, "", Taper::Linear),
    f("drive", "Drive", 0.1, 4.0, 1.0, "", Taper::Linear),
    e("filter_mode", "Filter Mode", FILTER_MODE_LABELS, 0.0),
    e("filter_slope", "Filter Slope", SLOPE_LABELS, 1.0),
    f(
        "hpf_cutoff",
        "HPF Cutoff",
        20.0,
        18000.0,
        20.0,
        "Hz",
        Taper::Exp { mid: 1000.0 },
    ),
    b("filter_key_track", "Key Track", 0.0),
    f(
        "env1_attack",
        "Env 1 Attack",
        0.001,
        10.0,
        0.005,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    f(
        "env1_decay",
        "Env 1 Decay",
        0.001,
        10.0,
        0.3,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    f(
        "env1_sustain",
        "Env 1 Sustain",
        0.0,
        1.0,
        0.0,
        "",
        Taper::Linear,
    ),
    f(
        "env1_release",
        "Env 1 Release",
        0.001,
        10.0,
        0.3,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    e("env1_shape", "Env 1 Shape", SHAPE_LABELS, 0.0),
    f(
        "env2_attack",
        "Env 2 Attack",
        0.001,
        10.0,
        0.005,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    f(
        "env2_decay",
        "Env 2 Decay",
        0.001,
        10.0,
        0.2,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    f(
        "env2_sustain",
        "Env 2 Sustain",
        0.0,
        1.0,
        0.8,
        "",
        Taper::Linear,
    ),
    f(
        "env2_release",
        "Env 2 Release",
        0.001,
        10.0,
        0.3,
        "s",
        Taper::Exp { mid: 1.0 },
    ),
    e("env2_shape", "Env 2 Shape", SHAPE_LABELS, 1.0),
    lfosel("amp_lfo_src", "Amp LFO", 0.0),
    f(
        "amp_lfo_depth",
        "Amp LFO Dep",
        0.0,
        1.0,
        0.0,
        "",
        Taper::Linear,
    ),
    b("amp_env_bypass", "Amp Gate", 0.0),
    e("lfo_shape", "LFO 1 Shape", LFO_LABELS, 0.0),
    f(
        "lfo_rate",
        "LFO 1 Rate",
        0.01,
        40.0,
        5.0,
        "Hz",
        Taper::Exp { mid: 5.0 },
    ),
    b("lfo_sync", "LFO 1 Sync", 0.0),
    f(
        "lfo1_delay_time",
        "LFO 1 Delay",
        0.0,
        4.0,
        0.0,
        "s",
        Taper::Linear,
    ),
    f("lfo1_fade", "LFO 1 Fade", 0.0, 4.0, 0.0, "s", Taper::Linear),
    b("lfo1_free_run", "LFO 1 Free", 0.0),
    lfosel("pitch_lfo_src", "Pitch LFO", 1.0),
    mp_vib_lfo("pitch_lfo_depth", "Pitch LFO Dep", 0.05),
    b("pitch_lfo_mod_only", "Pitch LFO Mod", 0.0),
    envsel("pitch_env_src", "Pitch Env"),
    mp_vib("pitch_env_depth", "Pitch Env Dep", 0.0),
    b("pitch_env_mod_only", "Pitch Env Mod", 0.0),
    f(
        "pitch_wheel_depth",
        "Pitch Wheel",
        0.0,
        12.0,
        2.0,
        "st",
        Taper::Linear,
    ),
    lfosel("pwm_lfo_src", "PWM LFO", 0.0),
    mwu("pwm_lfo_depth", "PWM LFO Dep"),
    envsel("pwm_env_src", "PWM Env"),
    mw("pwm_env_depth", "PWM Env Dep"),
    mcu("cutoff_lfo1_depth", "Cutoff LFO1 Dep"),
    mcu("cutoff_lfo2_depth", "Cutoff LFO2 Dep"),
    mc("cutoff_env_depth", "Cutoff Env Dep"),
    mc("vel_cutoff_depth", "Vel→Cutoff"),
    mw("mod_wheel_pwm", "Wheel→PWM"),
    mc("mod_wheel_cutoff", "Wheel→Cutoff"),
    f(
        "mod_wheel_reso",
        "Wheel→Reso",
        0.0,
        1.0,
        0.0,
        "",
        Taper::Linear,
    ),
    mp_wide("mod_wheel_cross_mod_sweep", "Wheel→X-Mod"),
    e("assign_mode", "Assign", ASSIGN_LABELS, 0.0),
    b("legato", "Legato", 0.0),
    f(
        "unison_detune",
        "Detune",
        0.0,
        50.0,
        12.0,
        "ct",
        Taper::Linear,
    ),
    f(
        "portamento_time",
        "Glide Time",
        0.0,
        0.5,
        0.0,
        "s",
        Taper::Exp { mid: 0.1 },
    ),
];

pub static GLOBAL_PARAMS: [ParamDesc; GlobalParam::COUNT] = [
    f(
        "master_tune",
        "Master Tune",
        -12.0,
        12.0,
        0.0,
        "st",
        Taper::Linear,
    ),
    f("master_volume", "Volume", 0.0, 1.0, 0.7, "", Taper::Linear),
    b("chorus_on", "Chorus", 1.0),
    f(
        "chorus_rate",
        "Chorus Rate",
        0.05,
        8.0,
        0.6,
        "Hz",
        Taper::Linear,
    ),
    f(
        "chorus_depth",
        "Chorus Depth",
        0.0,
        1.0,
        0.5,
        "",
        Taper::Linear,
    ),
    f("chorus_mix", "Chorus Mix", 0.0, 1.0, 0.4, "", Taper::Linear),
    b("delay_on", "Delay", 0.0),
    f(
        "delay_time",
        "Delay Time",
        0.01,
        2.0,
        0.35,
        "s",
        Taper::Linear,
    ),
    f(
        "delay_feedback",
        "Delay FB",
        0.0,
        0.95,
        0.4,
        "",
        Taper::Linear,
    ),
    f("delay_mix", "Delay Mix", 0.0, 1.0, 0.25, "", Taper::Linear),
    b("delay_pingpong", "Ping-Pong", 1.0),
    b("delay_sync", "Delay Sync", 0.0),
    b("reverb_on", "Reverb", 0.0),
    e("reverb_type", "Reverb Type", REVERB_TYPE_LABELS, 0.0),
    f("reverb_depth", "Reverb Depth", 0.0, 1.0, 0.5, "", Taper::Linear),
    f("reverb_mix", "Reverb Mix", 0.0, 1.0, 0.3, "", Taper::Linear),
    b("limiter_on", "Limiter", 0.0),
    e("oversample", "Oversample", OVERSAMPLE_LABELS, 1.0),
    e("lfo2_shape", "LFO 2 Shape", LFO_LABELS, 0.0),
    f(
        "lfo2_rate",
        "LFO 2 Rate",
        0.01,
        40.0,
        5.0,
        "Hz",
        Taper::Exp { mid: 5.0 },
    ),
    b("lfo2_sync", "LFO 2 Sync", 0.0),
];

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
}
