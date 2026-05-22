//! VXN1 parameter model.
//!
//! Parameters are a flat, index-addressed table so the same definition serves
//! the engine (reads plain values), the CLAP layer (stable integer ids =
//! indices, automation, save/restore) and the UI (labels, ranges, formatting).
//!
//! Values are stored as `f32` in *plain* units (Hz, seconds, semitones, …),
//! matching CLAP's plain-value convention. Enum/bool params store the variant
//! index / 0.0|1.0 and are read back through typed accessors.
//!
//! The 20 modulation-depth params (`Env1Pitch` … `KeyPwm`) are laid out
//! source-major, destination-minor so the engine can address them by
//! `MATRIX_BASE + source*ModDest::COUNT + dest` (see [`crate::modmatrix`]).

use crate::modmatrix::{ModDest, ModSource};
use vxn_dsp::{AdsrShape, LadderVariant, LfoShape, NoiseColor, Waveform};

/// Stable parameter identifiers. Discriminant = CLAP param id = table index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ParamId {
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
    // LFO
    LfoShape,
    LfoRate,
    // Global
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

impl ParamId {
    pub const COUNT: usize = ParamId::Oversample as usize + 1;

    /// Index of the first modulation-matrix parameter (`Env1Pitch`).
    pub const MATRIX_BASE: usize = ParamId::Env1Pitch as usize;

    pub fn all() -> impl Iterator<Item = ParamId> {
        (0..Self::COUNT).map(|i| Self::from_index(i).unwrap())
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<ParamId> {
        if i < Self::COUNT {
            Some(unsafe { std::mem::transmute::<usize, ParamId>(i) })
        } else {
            None
        }
    }

    /// Table index of the depth param for a `(source, destination)` route.
    #[inline]
    pub fn matrix_index(src: ModSource, dest: ModDest) -> usize {
        Self::MATRIX_BASE + (src as usize) * ModDest::COUNT + (dest as usize)
    }

    pub fn desc(self) -> &'static ParamDesc {
        &PARAMS[self.index()]
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    Float { unit: &'static str, log: bool },
    Int { unit: &'static str },
    Bool,
    Enum { variants: &'static [&'static str] },
}

#[derive(Clone, Copy, Debug)]
pub struct ParamDesc {
    pub id: ParamId,
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
}

const WAVE_LABELS: &[&str] = &["Sine", "Triangle", "Saw", "Pulse"];
const NOISE_LABELS: &[&str] = &["White", "Pink", "Brown"];
const VARIANT_LABELS: &[&str] = &["Sharp", "Smooth"];
const SHAPE_LABELS: &[&str] = &["Linear", "Exponential"];
const LFO_LABELS: &[&str] = &["Sine", "Tri", "Saw+", "Saw-", "Square", "S&H"];
const OVERSAMPLE_LABELS: &[&str] = &["Off", "2x", "4x"];

#[allow(clippy::too_many_arguments)]
const fn f(id: ParamId, name: &'static str, label: &'static str, min: f32, max: f32, default: f32, unit: &'static str, log: bool) -> ParamDesc {
    ParamDesc { id, name, label, min, max, default, kind: ParamKind::Float { unit, log } }
}
const fn e(id: ParamId, name: &'static str, label: &'static str, variants: &'static [&'static str], default: f32) -> ParamDesc {
    ParamDesc { id, name, label, min: 0.0, max: (variants.len() - 1) as f32, default, kind: ParamKind::Enum { variants } }
}
const fn b(id: ParamId, name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    ParamDesc { id, name, label, min: 0.0, max: 1.0, default, kind: ParamKind::Bool }
}
const fn i(id: ParamId, name: &'static str, label: &'static str, min: f32, max: f32, default: f32, unit: &'static str) -> ParamDesc {
    ParamDesc { id, name, label, min, max, default, kind: ParamKind::Int { unit } }
}
/// Pitch-destination depth param (semitones).
const fn mp(id: ParamId, name: &'static str, label: &'static str) -> ParamDesc {
    f(id, name, label, -48.0, 48.0, 0.0, "st", false)
}
/// Cutoff-destination depth param (semitones of cutoff).
const fn mc(id: ParamId, name: &'static str, label: &'static str) -> ParamDesc {
    f(id, name, label, -96.0, 96.0, 0.0, "st", false)
}
/// Amp-destination depth param (gain). `default` lets ENV-2→Amp seed to 1.0.
const fn ma(id: ParamId, name: &'static str, label: &'static str, default: f32) -> ParamDesc {
    f(id, name, label, -1.0, 1.0, default, "", false)
}
/// PWM-destination depth param (pulse-width fraction).
const fn mw(id: ParamId, name: &'static str, label: &'static str) -> ParamDesc {
    f(id, name, label, -0.5, 0.5, 0.0, "", false)
}

pub static PARAMS: [ParamDesc; ParamId::COUNT] = {
    use ParamId::*;
    [
        e(Osc1Wave, "osc1_wave", "Osc 1 Wave", WAVE_LABELS, 2.0),
        i(Osc1Coarse, "osc1_coarse", "Osc 1 Coarse", -24.0, 24.0, 0.0, "st"),
        f(Osc1Fine, "osc1_fine", "Osc 1 Fine", -50.0, 50.0, 0.0, "ct", false),
        f(Osc1Level, "osc1_level", "Osc 1 Level", 0.0, 1.0, 0.8, "", false),
        f(Osc1PulseWidth, "osc1_pw", "Osc 1 PW", 0.05, 0.95, 0.5, "", false),
        e(Osc2Wave, "osc2_wave", "Osc 2 Wave", WAVE_LABELS, 2.0),
        i(Osc2Coarse, "osc2_coarse", "Osc 2 Coarse", -24.0, 24.0, -12.0, "st"),
        f(Osc2Fine, "osc2_fine", "Osc 2 Fine", -50.0, 50.0, 7.0, "ct", false),
        f(Osc2Level, "osc2_level", "Osc 2 Level", 0.0, 1.0, 0.6, "", false),
        f(Osc2PulseWidth, "osc2_pw", "Osc 2 PW", 0.05, 0.95, 0.5, "", false),
        e(NoiseColor, "noise_color", "Noise Color", NOISE_LABELS, 0.0),
        f(NoiseLevel, "noise_level", "Noise Level", 0.0, 1.0, 0.0, "", false),
        f(Cutoff, "cutoff", "Cutoff", 20.0, 18000.0, 8000.0, "Hz", true),
        f(Resonance, "resonance", "Resonance", 0.0, 1.0, 0.2, "", false),
        f(Drive, "drive", "Drive", 0.1, 4.0, 1.0, "", false),
        e(FilterVariant, "filter_variant", "Filter Type", VARIANT_LABELS, 0.0),
        f(Env1Attack, "env1_attack", "Env 1 Attack", 0.001, 10.0, 0.005, "s", true),
        f(Env1Decay, "env1_decay", "Env 1 Decay", 0.001, 10.0, 0.3, "s", true),
        f(Env1Sustain, "env1_sustain", "Env 1 Sustain", 0.0, 1.0, 0.0, "", false),
        f(Env1Release, "env1_release", "Env 1 Release", 0.001, 10.0, 0.3, "s", true),
        e(Env1Shape, "env1_shape", "Env 1 Shape", SHAPE_LABELS, 0.0),
        f(Env2Attack, "env2_attack", "Env 2 Attack", 0.001, 10.0, 0.005, "s", true),
        f(Env2Decay, "env2_decay", "Env 2 Decay", 0.001, 10.0, 0.2, "s", true),
        f(Env2Sustain, "env2_sustain", "Env 2 Sustain", 0.0, 1.0, 0.8, "", false),
        f(Env2Release, "env2_release", "Env 2 Release", 0.001, 10.0, 0.3, "s", true),
        e(Env2Shape, "env2_shape", "Env 2 Shape", SHAPE_LABELS, 1.0),
        // Modulation matrix (source-major, dest-minor). ENV-2→Amp seeds to 1.0.
        mp(Env1Pitch, "env1_pitch", "Env1→Pitch"),
        mc(Env1Cutoff, "env1_cutoff", "Env1→Cutoff"),
        ma(Env1Amp, "env1_amp", "Env1→Amp", 0.0),
        mw(Env1Pwm, "env1_pwm", "Env1→PWM"),
        mp(Env2Pitch, "env2_pitch", "Env2→Pitch"),
        mc(Env2Cutoff, "env2_cutoff", "Env2→Cutoff"),
        ma(Env2Amp, "env2_amp", "Env2→Amp", 1.0),
        mw(Env2Pwm, "env2_pwm", "Env2→PWM"),
        mp(LfoPitch, "lfo_pitch", "LFO→Pitch"),
        mc(LfoCutoff, "lfo_cutoff", "LFO→Cutoff"),
        ma(LfoAmp, "lfo_amp", "LFO→Amp", 0.0),
        mw(LfoPwm, "lfo_pwm", "LFO→PWM"),
        mp(VelPitch, "vel_pitch", "Vel→Pitch"),
        mc(VelCutoff, "vel_cutoff", "Vel→Cutoff"),
        ma(VelAmp, "vel_amp", "Vel→Amp", 0.0),
        mw(VelPwm, "vel_pwm", "Vel→PWM"),
        mp(KeyPitch, "key_pitch", "Key→Pitch"),
        mc(KeyCutoff, "key_cutoff", "Key→Cutoff"),
        ma(KeyAmp, "key_amp", "Key→Amp", 0.0),
        mw(KeyPwm, "key_pwm", "Key→PWM"),
        e(LfoShape, "lfo_shape", "LFO Shape", LFO_LABELS, 0.0),
        f(LfoRate, "lfo_rate", "LFO Rate", 0.01, 40.0, 5.0, "Hz", true),
        f(MasterTune, "master_tune", "Master Tune", -12.0, 12.0, 0.0, "st", false),
        f(MasterVolume, "master_volume", "Volume", 0.0, 1.0, 0.7, "", false),
        b(ChorusOn, "chorus_on", "Chorus", 1.0),
        f(ChorusRate, "chorus_rate", "Chorus Rate", 0.05, 8.0, 0.6, "Hz", true),
        f(ChorusDepth, "chorus_depth", "Chorus Depth", 0.0, 1.0, 0.5, "", false),
        f(ChorusMix, "chorus_mix", "Chorus Mix", 0.0, 1.0, 0.4, "", false),
        b(DelayOn, "delay_on", "Delay", 0.0),
        f(DelayTime, "delay_time", "Delay Time", 0.01, 2.0, 0.35, "s", true),
        f(DelayFeedback, "delay_feedback", "Delay FB", 0.0, 0.95, 0.4, "", false),
        f(DelayMix, "delay_mix", "Delay Mix", 0.0, 1.0, 0.25, "", false),
        b(DelayPingPong, "delay_pingpong", "Ping-Pong", 1.0),
        e(Oversample, "oversample", "Oversample", OVERSAMPLE_LABELS, 1.0),
    ]
};

#[derive(Clone)]
pub struct ParamValues {
    v: [f32; ParamId::COUNT],
}

impl Default for ParamValues {
    fn default() -> Self {
        let mut v = [0.0; ParamId::COUNT];
        for (idx, d) in PARAMS.iter().enumerate() {
            v[idx] = d.default;
        }
        Self { v }
    }
}

impl ParamValues {
    #[inline]
    pub fn get(&self, id: ParamId) -> f32 {
        self.v[id.index()]
    }

    #[inline]
    pub fn get_index(&self, index: usize) -> f32 {
        self.v[index]
    }

    #[inline]
    pub fn set(&mut self, id: ParamId, value: f32) {
        self.v[id.index()] = id.desc().clamp(value);
    }

    #[inline]
    pub fn set_index(&mut self, index: usize, value: f32) {
        if let Some(id) = ParamId::from_index(index) {
            self.set(id, value);
        }
    }

    #[inline]
    pub fn bool(&self, id: ParamId) -> bool {
        self.get(id) >= 0.5
    }

    #[inline]
    fn enum_index(&self, id: ParamId, max: usize) -> usize {
        (self.get(id).round() as usize).min(max)
    }

    pub fn osc_wave(&self, id: ParamId) -> Waveform {
        Waveform::ALL[self.enum_index(id, Waveform::ALL.len() - 1)]
    }

    pub fn noise_color(&self) -> NoiseColor {
        NoiseColor::ALL[self.enum_index(ParamId::NoiseColor, NoiseColor::ALL.len() - 1)]
    }

    pub fn filter_variant(&self) -> LadderVariant {
        if self.enum_index(ParamId::FilterVariant, 1) == 0 {
            LadderVariant::Sharp
        } else {
            LadderVariant::Smooth
        }
    }

    pub fn lfo_shape(&self) -> LfoShape {
        LfoShape::ALL[self.enum_index(ParamId::LfoShape, LfoShape::ALL.len() - 1)]
    }

    /// Oversampling factor for the synthesis path: 1 (Off), 2 or 4.
    pub fn oversample_factor(&self) -> usize {
        match self.enum_index(ParamId::Oversample, 2) {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }

    pub fn env1_shape(&self) -> AdsrShape {
        self.adsr_shape(ParamId::Env1Shape)
    }

    pub fn env2_shape(&self) -> AdsrShape {
        self.adsr_shape(ParamId::Env2Shape)
    }

    fn adsr_shape(&self, id: ParamId) -> AdsrShape {
        if self.enum_index(id, 1) == 0 {
            AdsrShape::Linear
        } else {
            AdsrShape::Exponential
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_len_matches_count() {
        assert_eq!(PARAMS.len(), ParamId::COUNT);
    }

    #[test]
    fn ids_match_table_indices() {
        for (idx, d) in PARAMS.iter().enumerate() {
            assert_eq!(d.id.index(), idx, "param {} misindexed", d.name);
        }
    }

    #[test]
    fn from_index_roundtrips() {
        for id in ParamId::all() {
            assert_eq!(ParamId::from_index(id.index()), Some(id));
        }
        assert_eq!(ParamId::from_index(ParamId::COUNT), None);
    }

    #[test]
    fn defaults_in_range() {
        let p = ParamValues::default();
        for id in ParamId::all() {
            let d = id.desc();
            let val = p.get(id);
            assert!(val >= d.min && val <= d.max, "{} default OOR", d.name);
        }
    }

    #[test]
    fn matrix_layout_is_contiguous_and_ordered() {
        // The 20 matrix params must sit at MATRIX_BASE in source-major,
        // dest-minor order so matrix_index() addresses them correctly.
        assert_eq!(ParamId::MATRIX_BASE, ParamId::Env1Pitch.index());
        assert_eq!(ParamId::matrix_index(ModSource::Env2, ModDest::Amp), ParamId::Env2Amp.index());
        assert_eq!(ParamId::matrix_index(ModSource::KeyFollow, ModDest::Pwm), ParamId::KeyPwm.index());
        assert_eq!(ParamId::matrix_index(ModSource::Lfo, ModDest::Cutoff), ParamId::LfoCutoff.index());
        // ENV-2→Amp is the only route that defaults non-zero.
        assert_eq!(ParamValues::default().get(ParamId::Env2Amp), 1.0);
    }
}
