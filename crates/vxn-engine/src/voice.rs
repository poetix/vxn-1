//! A single synth voice: two oscillators + noise → mixer → ladder VCF → VCA,
//! with two assignable ADSR envelopes feeding a modulation matrix.
//!
//! Modulation model (Jupiter-8-shaped, generalised): ENV-1, ENV-2, the LFO,
//! velocity and key-follow are sources; pitch, cutoff, amp and PWM are
//! destinations. Pitch/cutoff/PWM are resolved once per control block (smooth
//! enough at 32 samples); amp is summed per sample so the VCA envelope stays
//! click-free.

use crate::modmatrix::{ModDest, ModMatrix};
use vxn_dsp::{
    AdsrCore, AdsrShape, LadderCoeffs, LadderKernel, LadderVariant, NoiseColor, NoiseSource,
    Oscillator, Waveform, fast_exp2, note_to_hz,
};

/// Everything a voice needs for one control block.
pub struct BlockCtx {
    /// Oversampled sample rate (`base_rate * oversample`). Oscillators and the
    /// filter run at this rate; envelopes run at the base rate.
    pub os_sample_rate: f32,
    /// Oversampling factor (1, 2 or 4): the number of audio samples produced
    /// per base-rate frame.
    pub os: usize,
    pub osc1_wave: Waveform,
    pub osc2_wave: Waveform,
    pub osc1_level: f32,
    pub osc2_level: f32,
    pub noise_level: f32,
    pub osc1_pw: f32,
    pub osc2_pw: f32,
    pub osc1_semi: f32,
    pub osc2_semi: f32,
    pub noise_color: NoiseColor,
    /// Base filter cutoff in Hz, before modulation.
    pub cutoff: f32,
    pub resonance: f32,
    pub drive: f32,
    pub variant: LadderVariant,
    /// Master tune + pitch bend, in semitones.
    pub base_semis: f32,
    /// LFO value for this block (bipolar `[-1, 1]`, held across the block).
    pub lfo_val: f32,
    pub matrix: ModMatrix,
}

pub struct Voice {
    osc1: Oscillator,
    osc2: Oscillator,
    noise: NoiseSource,
    ladder: LadderKernel,
    /// ENV-1: assignable (defaults unrouted).
    env1: AdsrCore,
    /// ENV-2: the conventional amp envelope (defaults ENV-2→Amp = 1).
    env2: AdsrCore,

    pub note: u8,
    velocity: f32,
    pub gate: bool,
    pub active: bool,
    trigger_pending: bool,
    pub alloc_tick: u64,
}

impl Voice {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            osc1: Oscillator::new(),
            osc2: Oscillator::new(),
            noise: NoiseSource::new(seed.wrapping_mul(2_654_435_761) | 1),
            ladder: LadderKernel::new(),
            env1: AdsrCore::new(sample_rate),
            env2: AdsrCore::new(sample_rate),
            note: 0,
            velocity: 0.0,
            gate: false,
            active: false,
            trigger_pending: false,
            alloc_tick: 0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.env1 = AdsrCore::new(sample_rate);
        self.env2 = AdsrCore::new(sample_rate);
        self.reset_state();
    }

    pub fn reset_state(&mut self) {
        self.osc1.reset();
        self.osc2.reset();
        self.noise.reset();
        self.ladder.reset();
        self.env1.reset();
        self.env2.reset();
        self.active = false;
        self.gate = false;
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, alloc_tick: u64) {
        self.note = note;
        self.velocity = velocity;
        self.gate = true;
        self.active = true;
        self.trigger_pending = true;
        self.alloc_tick = alloc_tick;
        self.osc1.reset();
        self.osc2.reset();
    }

    pub fn note_off(&mut self) {
        self.gate = false;
    }

    /// Apply envelope parameters. The engine calls this only when an envelope
    /// param actually changes (the derived coefficients need an `exp()` per
    /// segment), so the per-block render loop stays free of transcendentals.
    pub fn set_envelopes(
        &mut self,
        env1: (f32, f32, f32, f32),
        env1_shape: AdsrShape,
        env2: (f32, f32, f32, f32),
        env2_shape: AdsrShape,
    ) {
        self.env1.set_params(env1.0, env1.1, env1.2, env1.3);
        self.env1.set_shape(env1_shape);
        self.env2.set_params(env2.0, env2.1, env2.2, env2.3);
        self.env2.set_shape(env2_shape);
    }

    #[inline]
    pub fn is_free(&self) -> bool {
        !self.active
    }

    /// Key-follow source value: octaves relative to middle C (note 60).
    #[inline]
    fn key_follow(&self) -> f32 {
        (self.note as f32 - 60.0) / 12.0
    }

    /// Render one control block, accumulating into the oversampled `out` buffer
    /// (mono, length = `base_frames * ctx.os`).
    ///
    /// Envelopes tick once per base frame; the oscillators and filter run at the
    /// oversampled rate (`ctx.os` samples per base frame). The amp envelope is
    /// held across the oversampled subframes (it is far slower than audio rate).
    pub fn render_block(&mut self, out: &mut [f32], ctx: &BlockCtx) {
        if !self.active {
            return;
        }

        let kf = self.key_follow();
        let lfo = ctx.lfo_val;
        let vel = self.velocity;
        let os = ctx.os;

        // Block-start source vector (env levels sampled now). Order must match
        // ModSource: [Env1, Env2, Lfo, Velocity, KeyFollow].
        let srcs0 = [self.env1.level, self.env2.level, lfo, vel, kf];
        let pitch_mod = ctx.matrix.dest(ModDest::Pitch, &srcs0);
        let cutoff_mod = ctx.matrix.dest(ModDest::Cutoff, &srcs0);
        let pwm_mod = ctx.matrix.dest(ModDest::Pwm, &srcs0);

        let note = self.note as f32;
        let semis1 = ctx.base_semis + note + ctx.osc1_semi + pitch_mod;
        let semis2 = ctx.base_semis + note + ctx.osc2_semi + pitch_mod;
        // Increments are relative to the oversampled rate.
        self.osc1.set_increment(note_to_hz(semis1) / ctx.os_sample_rate);
        self.osc2.set_increment(note_to_hz(semis2) / ctx.os_sample_rate);
        self.osc1.pulse_width = (ctx.osc1_pw + pwm_mod).clamp(0.05, 0.95);
        self.osc2.pulse_width = (ctx.osc2_pw + pwm_mod).clamp(0.05, 0.95);

        // Cutoff modulation is in semitones above the base cutoff; coeffs at the
        // oversampled rate.
        let cutoff_hz = ctx.cutoff * fast_exp2(cutoff_mod / 12.0);
        self.ladder.set_coeffs(LadderCoeffs::new(
            cutoff_hz,
            ctx.os_sample_rate,
            ctx.resonance,
            ctx.drive,
            ctx.variant,
        ));

        let trig_at_start = std::mem::take(&mut self.trigger_pending);
        let base_frames = out.len() / os;

        for base_i in 0..base_frames {
            let trg = trig_at_start && base_i == 0;
            let e1 = self.env1.tick(trg, self.gate);
            let e2 = self.env2.tick(trg, self.gate);
            // VCA gain = the Amp column of the matrix; held across oversampled
            // subframes. ENV-2→Amp defaults to 1.0.
            let amp = ctx.matrix.dest(ModDest::Amp, &[e1, e2, lfo, vel, kf]).max(0.0);

            let frame = base_i * os;
            for k in 0..os {
                let s1 = self.osc1.next(ctx.osc1_wave) * ctx.osc1_level;
                let s2 = self.osc2.next(ctx.osc2_wave) * ctx.osc2_level;
                let nz = self.noise.next(ctx.noise_color) * ctx.noise_level;
                let filtered = self.ladder.tick(s1 + s2 + nz);
                out[frame + k] += filtered * amp;
            }
        }

        // Free the voice once both envelopes have released and the gate is off.
        if !self.gate && self.env1.is_idle() && self.env2.is_idle() {
            self.active = false;
        }
    }
}
