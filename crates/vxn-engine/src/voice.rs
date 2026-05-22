//! A single synth voice: two oscillators + noise → mixer → ladder VCF → VCA,
//! with per-voice amplitude and pitch (frequency) envelopes.
//!
//! Control-rate quantities arrive resolved in [`BlockCtx`] (computed once per
//! control block by the engine); the voice runs a tight per-sample inner loop.

use vxn_dsp::{
    AdsrCore, AdsrShape, LadderCoeffs, LadderKernel, LadderVariant, NoiseColor, NoiseSource,
    Oscillator, Waveform, note_to_hz,
};

/// Everything a voice needs for one control block. Built once, shared by all
/// voices so per-voice work stays minimal.
pub struct BlockCtx {
    pub sample_rate: f32,
    pub osc1_wave: Waveform,
    pub osc2_wave: Waveform,
    pub osc1_level: f32,
    pub osc2_level: f32,
    pub noise_level: f32,
    pub osc1_pw: f32,
    pub osc2_pw: f32,
    /// Coarse+fine pitch offset per oscillator, in semitones.
    pub osc1_semi: f32,
    pub osc2_semi: f32,
    pub noise_color: NoiseColor,
    pub cutoff: f32,
    pub resonance: f32,
    pub drive: f32,
    pub variant: LadderVariant,
    pub pitch_env_amt: f32,
    /// LFO pitch contribution this block, in semitones (already × LFO value).
    pub lfo_pitch_semis: f32,
    /// Amplitude modulation multiplier this block (tremolo).
    pub amp_mod: f32,
    /// Master tune + pitch bend, in semitones.
    pub base_semis: f32,
    // Envelope parameters (applied each block; cheap).
    pub amp_adsr: (f32, f32, f32, f32),
    pub amp_shape: AdsrShape,
    pub pitch_adsr: (f32, f32, f32, f32),
    pub pitch_shape: AdsrShape,
}

pub struct Voice {
    osc1: Oscillator,
    osc2: Oscillator,
    noise: NoiseSource,
    ladder: LadderKernel,
    amp_env: AdsrCore,
    pitch_env: AdsrCore,

    pub note: u8,
    velocity: f32,
    pub gate: bool,
    pub active: bool,
    trigger_pending: bool,
    /// Monotonic stamp set at allocation; used for voice stealing.
    pub alloc_tick: u64,
}

impl Voice {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            osc1: Oscillator::new(),
            osc2: Oscillator::new(),
            noise: NoiseSource::new(seed.wrapping_mul(2_654_435_761) | 1),
            ladder: LadderKernel::new(),
            amp_env: AdsrCore::new(sample_rate),
            pitch_env: AdsrCore::new(sample_rate),
            note: 0,
            velocity: 0.0,
            gate: false,
            active: false,
            trigger_pending: false,
            alloc_tick: 0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.amp_env = AdsrCore::new(sample_rate);
        self.pitch_env = AdsrCore::new(sample_rate);
        self.reset_state();
    }

    pub fn reset_state(&mut self) {
        self.osc1.reset();
        self.osc2.reset();
        self.noise.reset();
        self.ladder.reset();
        self.amp_env.reset();
        self.pitch_env.reset();
        self.active = false;
        self.gate = false;
    }

    /// Start a note. Phases reset (Jupiter-style DCO behaviour); envelopes
    /// retrigger from their current level.
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

    /// True once fully released and safe to reallocate.
    #[inline]
    pub fn is_free(&self) -> bool {
        !self.active
    }

    /// Render one control block, accumulating into `out` (mono).
    pub fn render_block(&mut self, out: &mut [f32], ctx: &BlockCtx) {
        if !self.active {
            return;
        }

        // Envelope parameters (cheap to re-apply each block).
        self.amp_env.set_params(ctx.amp_adsr.0, ctx.amp_adsr.1, ctx.amp_adsr.2, ctx.amp_adsr.3);
        self.amp_env.set_shape(ctx.amp_shape);
        self.pitch_env.set_params(ctx.pitch_adsr.0, ctx.pitch_adsr.1, ctx.pitch_adsr.2, ctx.pitch_adsr.3);
        self.pitch_env.set_shape(ctx.pitch_shape);

        // Pitch resolved at block start (pitch env sampled here; LFO held).
        let pitch_mod = self.pitch_env.level * ctx.pitch_env_amt + ctx.lfo_pitch_semis;
        let note = self.note as f32;
        let semis1 = ctx.base_semis + note + ctx.osc1_semi + pitch_mod;
        let semis2 = ctx.base_semis + note + ctx.osc2_semi + pitch_mod;
        self.osc1.set_increment(note_to_hz(semis1) / ctx.sample_rate);
        self.osc2.set_increment(note_to_hz(semis2) / ctx.sample_rate);
        self.osc1.pulse_width = ctx.osc1_pw;
        self.osc2.pulse_width = ctx.osc2_pw;

        self.ladder.set_coeffs(LadderCoeffs::new(
            ctx.cutoff,
            ctx.sample_rate,
            ctx.resonance,
            ctx.drive,
            ctx.variant,
        ));

        let trig_at_start = std::mem::take(&mut self.trigger_pending);
        let vel = self.velocity;

        for (i, slot) in out.iter_mut().enumerate() {
            let trg = trig_at_start && i == 0;
            let s1 = self.osc1.next(ctx.osc1_wave) * ctx.osc1_level;
            let s2 = self.osc2.next(ctx.osc2_wave) * ctx.osc2_level;
            let nz = self.noise.next(ctx.noise_color) * ctx.noise_level;
            let mixed = s1 + s2 + nz;
            let filtered = self.ladder.tick(mixed);
            let ae = self.amp_env.tick(trg, self.gate);
            self.pitch_env.tick(trg, self.gate);
            *slot += filtered * ae * ctx.amp_mod * vel;
        }

        // Free the voice once the amplitude envelope has fully released.
        if self.amp_env.is_idle() {
            self.active = false;
        }
    }
}
