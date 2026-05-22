//! VXN1 parameter model.
//!
//! Parameters are a flat, index-addressed table so the same definition serves
//! the engine (reads plain values), the CLAP layer (stable integer ids =
//! indices, automation, save/restore) and the UI (labels, ranges, formatting).
//!
//! Values are stored as `f32` in *plain* units (Hz, seconds, semitones, …),
//! matching CLAP's plain-value convention. Enum/bool params store the variant
//! index / 0.0|1.0 and are read back through typed accessors.

use vxn_dsp::{LadderVariant, LfoShape, NoiseColor, Waveform};

/// Stable parameter identifiers. Discriminant = CLAP param id = table index.
/// Append-only: never reorder or remove (would break saved automation).
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
    // Amplitude envelope
    AmpAttack,
    AmpDecay,
    AmpSustain,
    AmpRelease,
    AmpShape,
    // Pitch (frequency) envelope
    PitchAttack,
    PitchDecay,
    PitchSustain,
    PitchRelease,
    PitchShape,
    PitchEnvAmount,
    // LFO
    LfoShape,
    LfoRate,
    LfoToAmp,
    LfoToBaseFreq,
    LfoToPitch,
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
}

impl ParamId {
    /// Total parameter count.
    pub const COUNT: usize = ParamId::DelayPingPong as usize + 1;

    /// All ids in table order.
    pub fn all() -> impl Iterator<Item = ParamId> {
        (0..Self::COUNT).map(|i| Self::from_index(i).unwrap())
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Option<ParamId> {
        if i < Self::COUNT {
            // Safe: ParamId is repr(usize), contiguous 0..COUNT.
            Some(unsafe { std::mem::transmute::<usize, ParamId>(i) })
        } else {
            None
        }
    }

    pub fn desc(self) -> &'static ParamDesc {
        &PARAMS[self.index()]
    }
}

/// Parameter value kind, with the metadata the UI/host need.
#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    /// Continuous. `log` requests logarithmic mapping in the UI (e.g. cutoff).
    Float { unit: &'static str, log: bool },
    /// Integer steps (inclusive range).
    Int { unit: &'static str },
    Bool,
    /// Enumerated choice; `variants` are the display labels.
    Enum { variants: &'static [&'static str] },
}

/// Static description of one parameter.
#[derive(Clone, Copy, Debug)]
pub struct ParamDesc {
    pub id: ParamId,
    /// Stable machine name (for state save/restore by name).
    pub name: &'static str,
    /// Human label for the UI.
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

    /// Plain value → normalised `[0, 1]` (linear; log mapping is a UI concern).
    #[inline]
    pub fn to_normalized(&self, v: f32) -> f32 {
        if self.max > self.min {
            ((v - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Normalised `[0, 1]` → plain value.
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

/// The full parameter table. Order must match [`ParamId`] discriminants.
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
        f(AmpAttack, "amp_attack", "Amp Attack", 0.001, 10.0, 0.005, "s", true),
        f(AmpDecay, "amp_decay", "Amp Decay", 0.001, 10.0, 0.2, "s", true),
        f(AmpSustain, "amp_sustain", "Amp Sustain", 0.0, 1.0, 0.8, "", false),
        f(AmpRelease, "amp_release", "Amp Release", 0.001, 10.0, 0.3, "s", true),
        e(AmpShape, "amp_shape", "Amp Shape", SHAPE_LABELS, 1.0),
        f(PitchAttack, "pitch_attack", "Pitch Attack", 0.001, 10.0, 0.005, "s", true),
        f(PitchDecay, "pitch_decay", "Pitch Decay", 0.001, 10.0, 0.2, "s", true),
        f(PitchSustain, "pitch_sustain", "Pitch Sustain", 0.0, 1.0, 0.0, "", false),
        f(PitchRelease, "pitch_release", "Pitch Release", 0.001, 10.0, 0.3, "s", true),
        e(PitchShape, "pitch_shape", "Pitch Shape", SHAPE_LABELS, 0.0),
        f(PitchEnvAmount, "pitch_env_amt", "Pitch Env Amt", -48.0, 48.0, 0.0, "st", false),
        e(LfoShape, "lfo_shape", "LFO Shape", LFO_LABELS, 0.0),
        f(LfoRate, "lfo_rate", "LFO Rate", 0.01, 40.0, 5.0, "Hz", true),
        f(LfoToAmp, "lfo_to_amp", "LFO→Amp", 0.0, 1.0, 0.0, "", false),
        f(LfoToBaseFreq, "lfo_to_basefreq", "LFO→Freq", 0.0, 12.0, 0.0, "st", false),
        f(LfoToPitch, "lfo_to_pitch", "LFO→Pitch", 0.0, 12.0, 0.0, "st", false),
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
    ]
};

/// Live parameter values in plain units. Cheap to clone (one array).
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

    pub fn amp_shape(&self) -> vxn_dsp::AdsrShape {
        self.adsr_shape(ParamId::AmpShape)
    }

    pub fn pitch_shape(&self) -> vxn_dsp::AdsrShape {
        self.adsr_shape(ParamId::PitchShape)
    }

    fn adsr_shape(&self, id: ParamId) -> vxn_dsp::AdsrShape {
        if self.enum_index(id, 1) == 0 {
            vxn_dsp::AdsrShape::Linear
        } else {
            vxn_dsp::AdsrShape::Exponential
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
}
