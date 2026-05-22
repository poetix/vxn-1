//! VXN1 synth engine.
//!
//! Framework-agnostic: holds parameters, allocates voices, and renders audio
//! in fixed control blocks. The CLAP layer drives it with note/param events
//! and contiguous output slices; the UI reads and writes [`ParamValues`].

pub mod params;
pub mod voice;

pub use params::{ParamDesc, ParamId, ParamKind, ParamValues, PARAMS};

use vxn_dsp::{CONTROL_BLOCK, LfoCore, MAX_VOICES, StereoChorus, StereoDelay, note_to_hz};
use voice::{BlockCtx, Voice};

/// Re-export so the plugin shell can flush denormals without depending on
/// `vxn-dsp` directly.
pub use vxn_dsp::enable_flush_to_zero;

/// The complete VXN1 instrument.
pub struct Synth {
    sample_rate: f32,
    params: ParamValues,
    voices: Vec<Voice>,
    lfo: LfoCore,
    chorus: StereoChorus,
    delay: StereoDelay,
    /// Pitch bend in semitones (±2 by default range).
    bend_semis: f32,
    alloc_counter: u64,
}

impl Synth {
    pub fn new(sample_rate: f32) -> Self {
        let voices = (0..MAX_VOICES)
            .map(|i| Voice::new(sample_rate, i as u64 + 1))
            .collect();
        // The LFO ticks once per control block, so its effective sample rate
        // is the control rate. Max LFO rate (40 Hz) still has ample steps/cycle.
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
        Self {
            sample_rate,
            params: ParamValues::default(),
            voices,
            lfo: LfoCore::new(control_rate, 0x51A7),
            chorus: StereoChorus::new(sample_rate),
            delay: StereoDelay::new(sample_rate, 2.0),
            bend_semis: 0.0,
            alloc_counter: 0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (sample_rate - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sample_rate;
        for v in &mut self.voices {
            v.set_sample_rate(sample_rate);
        }
        self.lfo = LfoCore::new(sample_rate / CONTROL_BLOCK as f32, 0x51A7);
        self.chorus = StereoChorus::new(sample_rate);
        self.delay = StereoDelay::new(sample_rate, 2.0);
    }

    pub fn params(&self) -> &ParamValues {
        &self.params
    }

    pub fn params_mut(&mut self) -> &mut ParamValues {
        &mut self.params
    }

    /// Set a parameter by CLAP id (= table index).
    pub fn set_param(&mut self, index: usize, value: f32) {
        self.params.set_index(index, value);
    }

    /// Pitch bend in normalised `[-1, 1]`; mapped to ±2 semitones.
    pub fn set_pitch_bend(&mut self, normalized: f32) {
        self.bend_semis = normalized.clamp(-1.0, 1.0) * 2.0;
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        self.alloc_counter += 1;
        let tick = self.alloc_counter;
        let idx = self.allocate_voice(note);
        self.voices[idx].note_on(note, velocity, tick);
    }

    pub fn note_off(&mut self, note: u8) {
        for v in &mut self.voices {
            if v.active && v.gate && v.note == note {
                v.note_off();
            }
        }
    }

    pub fn all_notes_off(&mut self) {
        for v in &mut self.voices {
            v.note_off();
        }
    }

    pub fn reset(&mut self) {
        for v in &mut self.voices {
            v.reset_state();
        }
        self.chorus.clear();
        self.delay.clear();
        self.lfo.reset();
    }

    /// Pick a voice: prefer a free one, else steal the oldest (lowest stamp).
    fn allocate_voice(&mut self, note: u8) -> usize {
        // Re-use a voice already playing this note (mono-legato within a key).
        if let Some(i) = self.voices.iter().position(|v| v.active && v.note == note) {
            return i;
        }
        if let Some(i) = self.voices.iter().position(|v| v.is_free()) {
            return i;
        }
        // Steal oldest.
        let mut best = 0;
        let mut best_tick = u64::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            if v.alloc_tick < best_tick {
                best_tick = v.alloc_tick;
                best = i;
            }
        }
        best
    }

    /// Render `out_l`/`out_r` (equal length). No events occur within this span;
    /// the caller splits the host buffer at event boundaries.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n = out_l.len().min(out_r.len());
        let mut start = 0;
        while start < n {
            let block = (n - start).min(CONTROL_BLOCK);
            let ctx = self.build_ctx();

            // Per-voice mono mix for this control block.
            let mut mono = [0.0f32; CONTROL_BLOCK];
            let mono = &mut mono[..block];
            for v in &mut self.voices {
                v.render_block(mono, &ctx);
            }

            // Effects (stereo), then write out.
            let chorus_on = self.params.bool(ParamId::ChorusOn);
            let delay_on = self.params.bool(ParamId::DelayOn);
            let volume = self.params.get(ParamId::MasterVolume);
            self.update_effects();

            for (i, &m) in mono.iter().enumerate() {
                let dry = m * volume;
                let (mut l, mut r) = (dry, dry);
                if chorus_on {
                    (l, r) = self.chorus.process(l, r);
                }
                if delay_on {
                    (l, r) = self.delay.process(l, r);
                }
                out_l[start + i] = l;
                out_r[start + i] = r;
            }
            start += block;
        }
    }

    fn update_effects(&mut self) {
        let p = &self.params;
        self.chorus.set_params(
            p.get(ParamId::ChorusRate),
            p.get(ParamId::ChorusDepth),
            p.get(ParamId::ChorusMix),
        );
        let t = p.get(ParamId::DelayTime);
        self.delay.set_params(
            t,
            t,
            p.get(ParamId::DelayFeedback),
            0.3,
            p.get(ParamId::DelayMix),
            p.bool(ParamId::DelayPingPong),
        );
    }

    fn build_ctx(&mut self) -> BlockCtx {
        let p = &self.params;
        let lfo_val = self.lfo.next(p.lfo_shape());
        self.lfo.set_rate(p.get(ParamId::LfoRate));

        let lfo_pitch_semis =
            lfo_val * (p.get(ParamId::LfoToBaseFreq) + p.get(ParamId::LfoToPitch));
        let amp_mod = (1.0 + lfo_val * p.get(ParamId::LfoToAmp)).max(0.0);

        BlockCtx {
            sample_rate: self.sample_rate,
            osc1_wave: p.osc_wave(ParamId::Osc1Wave),
            osc2_wave: p.osc_wave(ParamId::Osc2Wave),
            osc1_level: p.get(ParamId::Osc1Level),
            osc2_level: p.get(ParamId::Osc2Level),
            noise_level: p.get(ParamId::NoiseLevel),
            osc1_pw: p.get(ParamId::Osc1PulseWidth),
            osc2_pw: p.get(ParamId::Osc2PulseWidth),
            osc1_semi: p.get(ParamId::Osc1Coarse) + p.get(ParamId::Osc1Fine) / 100.0,
            osc2_semi: p.get(ParamId::Osc2Coarse) + p.get(ParamId::Osc2Fine) / 100.0,
            noise_color: p.noise_color(),
            cutoff: p.get(ParamId::Cutoff),
            resonance: p.get(ParamId::Resonance),
            drive: p.get(ParamId::Drive),
            variant: p.filter_variant(),
            pitch_env_amt: p.get(ParamId::PitchEnvAmount),
            lfo_pitch_semis,
            amp_mod,
            base_semis: p.get(ParamId::MasterTune) + self.bend_semis,
            amp_adsr: (
                p.get(ParamId::AmpAttack),
                p.get(ParamId::AmpDecay),
                p.get(ParamId::AmpSustain),
                p.get(ParamId::AmpRelease),
            ),
            amp_shape: p.amp_shape(),
            pitch_adsr: (
                p.get(ParamId::PitchAttack),
                p.get(ParamId::PitchDecay),
                p.get(ParamId::PitchSustain),
                p.get(ParamId::PitchRelease),
            ),
            pitch_shape: p.pitch_shape(),
        }
    }
}

/// Convenience: A4 = 440 Hz reference, exposed for tests/tools.
pub fn a4_hz() -> f32 {
    note_to_hz(69.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(synth: &mut Synth, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0; frames];
        let mut r = vec![0.0; frames];
        synth.process(&mut l, &mut r);
        (l, r)
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    #[test]
    fn a4_is_440() {
        assert!((a4_hz() - 440.0).abs() < 0.5, "A4 = {}", a4_hz());
    }

    #[test]
    fn silent_when_idle() {
        let mut s = Synth::new(48_000.0);
        let (l, _) = render(&mut s, 512);
        assert!(rms(&l) < 1e-6, "idle output not silent");
    }

    #[test]
    fn note_produces_sound_then_releases_to_silence() {
        let mut s = Synth::new(48_000.0);
        // Fast envelope so the test is short.
        s.set_param(ParamId::AmpAttack.index(), 0.001);
        s.set_param(ParamId::AmpRelease.index(), 0.01);
        s.set_param(ParamId::ChorusOn.index(), 0.0);
        s.note_on(69, 1.0);
        let (l, _) = render(&mut s, 4800);
        assert!(rms(&l) > 0.01, "note produced no sound");

        s.note_off(69);
        // Render well past the release.
        let (tail, _) = render(&mut s, 48_000);
        let last = &tail[tail.len() - 4800..];
        assert!(rms(last) < 1e-4, "did not release to silence: {}", rms(last));
    }

    #[test]
    fn output_finite_under_stress() {
        let mut s = Synth::new(44_100.0);
        s.set_param(ParamId::Resonance.index(), 1.0);
        s.set_param(ParamId::NoiseLevel.index(), 0.5);
        s.set_param(ParamId::DelayOn.index(), 1.0);
        for n in 60..76 {
            s.note_on(n, 1.0);
        }
        let (l, r) = render(&mut s, 44_100);
        assert!(l.iter().chain(r.iter()).all(|x| x.is_finite()), "non-finite output");
        let peak = l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak < 20.0, "output blew up: peak {peak}");
    }

    #[test]
    fn voice_stealing_keeps_polyphony_bounded() {
        let mut s = Synth::new(48_000.0);
        for n in 0..40u8 {
            s.note_on(n, 1.0);
        }
        let active = s.voices.iter().filter(|v| v.active).count();
        assert!(active <= MAX_VOICES, "too many active voices: {active}");
    }
}
