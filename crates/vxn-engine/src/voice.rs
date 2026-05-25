//! Structure-of-arrays voice bank: all 16 voices processed together so the
//! oscillator/filter/noise hot path vectorises across voices (see
//! `vxn_dsp::poly`). Envelopes stay scalar (one [`AdsrCore`] per voice) and
//! tick at the base rate; the oscillators and ladder run at the oversampled
//! rate.
//!
//! Modulation model (Jupiter-8-shaped, generalised): ENV-1, ENV-2, LFO,
//! velocity and key-follow are sources; pitch, cutoff, amp and PWM are
//! destinations. Pitch/cutoff/PWM are resolved once per control block; amp is
//! evaluated per base frame (held across oversampled subframes).

use vxn_dsp::{
    AdsrCore, AdsrShape, CHANNELS_PER_LAYER, LadderCoeffs, LadderVariant, NoiseColor, PolyHpf,
    PolyLadder, PolyNoise, PolyOscillator, Waveform, fast_exp2, note_to_hz,
};

use crate::modmatrix::{ModDest, ModMatrix, ModSource};
use crate::params::AssignMode;

/// One [`VoiceBank`] is a single layer: its channels render together as a
/// homogeneous group (ADR 0003 §10).
const N: usize = CHANNELS_PER_LAYER;

/// HPF cutoff at or below this (Hz) is treated as "off" and bypassed. Matches
/// the `HpfCutoff` param minimum (its default, ≈ fully open).
const HPF_OFF_HZ: f32 = 20.0;

/// Per-voice LFO modulation fade-in (the JP-8 "LFO delay"): each voice's gain
/// ramps 0→1 over a settable time after note-on, scaling the LFO source seen by
/// the matrix so every LFO-driven destination fades in together. One instance
/// per LFO — a second LFO is a second `LfoFadeIn`, no new ramp logic.
#[derive(Clone)]
struct LfoFadeIn {
    gain: [f32; N],
}

impl LfoFadeIn {
    fn new() -> Self {
        Self { gain: [0.0; N] }
    }

    fn reset(&mut self) {
        self.gain = [0.0; N];
    }

    /// Restart voice `v`'s fade from zero (call on note-on).
    #[inline]
    fn retrigger(&mut self, v: usize) {
        self.gain[v] = 0.0;
    }

    #[inline]
    fn gain(&self, v: usize) -> f32 {
        self.gain[v]
    }

    /// Prepare the block: returns the per-base-frame ramp increment for
    /// `delay_s`. A delay of 0 pins every voice to full depth and returns 0,
    /// reproducing the undelayed path exactly.
    #[inline]
    fn begin_block(&mut self, delay_s: f32, base_rate: f32) -> f32 {
        if delay_s > 0.0 {
            1.0 / (delay_s * base_rate)
        } else {
            self.gain = [1.0; N];
            0.0
        }
    }

    /// Advance every voice's fade one base frame toward full depth. `inc == 0`
    /// (no delay) is a no-op.
    #[inline]
    fn advance(&mut self, inc: f32) {
        if inc > 0.0 {
            for g in &mut self.gain {
                *g = (*g + inc).min(1.0);
            }
        }
    }
}

/// Control-block context shared by all voices.
pub struct BlockCtx {
    /// Oversampled sample rate (`base_rate * oversample`).
    pub os_sample_rate: f32,
    /// Oversampling factor (1, 2 or 4).
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
    pub cutoff: f32,
    /// Pre-VCF high-pass cutoff (Hz). 20 ≈ open / "off".
    pub hpf_cutoff: f32,
    pub resonance: f32,
    pub drive: f32,
    pub variant: LadderVariant,
    pub base_semis: f32,
    pub lfo_val: f32,
    /// LFO modulation fade-in time after note-on (s); 0 = no delay.
    pub lfo_delay: f32,
    /// Hard sync on: osc2 (slave) phase resets each osc1 (master) cycle. Off
    /// keeps the independent, vectorised osc fast path (E002 ticket 0004).
    pub sync: bool,
    /// Cross-mod / linear FM depth (osc2 → osc1 pitch). 0 = off. Engages the
    /// coupled path alongside `sync` (E002 ticket 0005).
    pub cross_mod: f32,
    /// Portamento (pitch glide) enabled for this layer.
    pub portamento_on: bool,
    /// Portamento glide time (s); 0 = instant. Glide is per channel, resolved at
    /// control-block rate so it feeds osc pitch, sync and cross-mod consistently.
    pub portamento_time: f32,
    pub matrix: ModMatrix,
}

/// All 16 voices in structure-of-arrays form.
pub struct VoiceBank {
    osc1: PolyOscillator,
    osc2: PolyOscillator,
    noise: PolyNoise,
    hpf: PolyHpf,
    ladder: PolyLadder,
    env1: [AdsrCore; N],
    env2: [AdsrCore; N],

    note: [u8; N],
    velocity: [f32; N],
    gate: [bool; N],
    active: [bool; N],
    trigger_pending: [bool; N],
    alloc_tick: [u64; N],
    /// Per-channel detune (cents), added to both oscillators. Zero for Poly;
    /// the Unison assign mode spreads channels with it.
    detune_cents: [f32; N],
    /// Output level compensation for the channel sum: 1.0 for Poly, ~1/√N for
    /// Unison so stacking all channels on one note isn't an N× level jump.
    level_comp: f32,
    /// Per-channel glided pitch (MIDI note as f32). With portamento it ramps
    /// toward the target note at control-block rate; without, it tracks the note.
    glide_semi: [f32; N],
    /// Whether a channel has a previous pitch to glide *from*. False until its
    /// first note, so the first note never sweeps up from zero.
    glide_valid: [bool; N],
    /// LFO modulation fade-in after note-on (`ctx.lfo_delay`).
    lfo_fade: LfoFadeIn,
}

impl VoiceBank {
    /// `noise_seed` differs per layer so two layers' noise generators are
    /// decorrelated (no comb artefacts when two similar patches sum).
    pub fn new(sample_rate: f32, noise_seed: u64) -> Self {
        Self {
            osc1: PolyOscillator::new(),
            osc2: PolyOscillator::new(),
            noise: PolyNoise::new(noise_seed),
            hpf: PolyHpf::new(),
            ladder: PolyLadder::new(),
            env1: std::array::from_fn(|_| AdsrCore::new(sample_rate)),
            env2: std::array::from_fn(|_| AdsrCore::new(sample_rate)),
            note: [0; N],
            velocity: [0.0; N],
            gate: [false; N],
            active: [false; N],
            trigger_pending: [false; N],
            alloc_tick: [0; N],
            detune_cents: [0.0; N],
            level_comp: 1.0,
            glide_semi: [0.0; N],
            glide_valid: [false; N],
            lfo_fade: LfoFadeIn::new(),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.env1 = std::array::from_fn(|_| AdsrCore::new(sample_rate));
        self.env2 = std::array::from_fn(|_| AdsrCore::new(sample_rate));
        self.reset_all();
    }

    pub fn reset_all(&mut self) {
        self.osc1 = PolyOscillator::new();
        self.osc2 = PolyOscillator::new();
        self.noise.reset();
        self.hpf.reset();
        self.ladder.reset();
        for e in &mut self.env1 {
            e.reset();
        }
        for e in &mut self.env2 {
            e.reset();
        }
        self.active = [false; N];
        self.gate = [false; N];
        self.detune_cents = [0.0; N];
        self.level_comp = 1.0;
        self.glide_semi = [0.0; N];
        self.glide_valid = [false; N];
        self.lfo_fade.reset();
    }

    pub fn active_count(&self) -> usize {
        self.active.iter().filter(|&&a| a).count()
    }

    /// Apply envelope params to every voice (called by the engine only when an
    /// envelope param changed).
    pub fn set_envelopes(
        &mut self,
        env1: (f32, f32, f32, f32),
        env1_shape: AdsrShape,
        env2: (f32, f32, f32, f32),
        env2_shape: AdsrShape,
    ) {
        for e in &mut self.env1 {
            e.set_params(env1.0, env1.1, env1.2, env1.3);
            e.set_shape(env1_shape);
        }
        for e in &mut self.env2 {
            e.set_params(env2.0, env2.1, env2.2, env2.3);
            e.set_shape(env2_shape);
        }
    }

    /// Start a note under assign mode `mode` — the per-layer MIDI processor seam
    /// (ADR 0003 §4). **Poly** allocates one channel (first-free / oldest-steal
    /// across the layer's 8). **Unison** stacks the note across all channels with
    /// per-channel detune (0011 fills the spread; here it stacks undetuned).
    /// Phases reset (DCO behaviour); envelopes retrigger from their current level.
    ///
    /// Arp hook (deferred, ADR 0003 §4): a future arpeggiator is a *stream
    /// transform before allocation* — it would turn held notes into a timed
    /// sequence and feed each step here as an ordinary `note_on`, so neither the
    /// event router (0009) nor the render path (0008) changes.
    pub fn note_on(
        &mut self,
        mode: AssignMode,
        note: u8,
        velocity: f32,
        alloc_tick: u64,
        unison_detune: f32,
    ) {
        match mode {
            AssignMode::Poly => {
                let v = self.allocate(note);
                self.trigger(v, note, velocity, alloc_tick, 0.0);
                self.level_comp = 1.0;
            }
            AssignMode::Unison => {
                // Last-note priority: every channel retriggers to the new note
                // (the prior note is not stacked). Per-channel detune fans the 8
                // copies out for chorusing thickness.
                for v in 0..N {
                    self.trigger(
                        v,
                        note,
                        velocity,
                        alloc_tick,
                        unison_spread(v) * unison_detune,
                    );
                }
                self.level_comp = 1.0 / (N as f32).sqrt();
            }
        }
    }

    /// Trigger a specific channel: the lowest level of the assign seam. Poly hits
    /// one channel, Unison hits all; both route through here so per-channel state
    /// (gate, detune, phase reset) is set in exactly one place.
    fn trigger(&mut self, v: usize, note: u8, velocity: f32, alloc_tick: u64, detune_cents: f32) {
        self.note[v] = note;
        self.velocity[v] = velocity;
        self.gate[v] = true;
        self.active[v] = true;
        self.trigger_pending[v] = true;
        self.alloc_tick[v] = alloc_tick;
        self.detune_cents[v] = detune_cents;
        self.lfo_fade.retrigger(v);
        self.osc1.reset(v);
        self.osc2.reset(v);
    }

    pub fn note_off(&mut self, note: u8) {
        for v in 0..N {
            if self.active[v] && self.gate[v] && self.note[v] == note {
                self.gate[v] = false;
            }
        }
    }

    pub fn all_notes_off(&mut self) {
        self.gate = [false; N];
    }

    /// Pick a voice: re-use one already playing this note, else a free voice,
    /// else steal the oldest.
    fn allocate(&self, note: u8) -> usize {
        if let Some(v) = (0..N).find(|&v| self.active[v] && self.note[v] == note) {
            return v;
        }
        if let Some(v) = (0..N).find(|&v| !self.active[v]) {
            return v;
        }
        let mut best = 0;
        let mut best_tick = u64::MAX;
        for v in 0..N {
            if self.alloc_tick[v] < best_tick {
                best_tick = self.alloc_tick[v];
                best = v;
            }
        }
        best
    }

    /// Assemble the modulation source vector for voice `v`, in [`ModSource`]
    /// order. The envelope levels are passed in because the two call sites
    /// differ — block-start resolution reads the stored ENV level, the per-frame
    /// amp path the just-ticked value — while velocity, key-follow and the
    /// fade-scaled LFO are common to both. A new source is added in one place.
    #[inline]
    fn mod_sources(
        &self,
        v: usize,
        ctx: &BlockCtx,
        env1: f32,
        env2: f32,
    ) -> [f32; ModSource::COUNT] {
        [
            env1,
            env2,
            ctx.lfo_val * self.lfo_fade.gain(v),
            self.velocity[v],
            key_follow(self.note[v]),
        ]
    }

    /// Render one control block into the oversampled mono buffer `out`
    /// (length = `base_frames * ctx.os`), accumulating all voices.
    pub fn render_block(&mut self, out: &mut [f32], ctx: &BlockCtx) {
        let os = ctx.os;
        let base_frames = out.len() / os;
        let base_rate = ctx.os_sample_rate / os as f32;
        let lfo_gain_inc = self.lfo_fade.begin_block(ctx.lfo_delay, base_rate);

        // Portamento glide coefficient for this block (one-pole toward the target
        // note). `dt` is the block's wall-clock duration, so the glide rate is
        // independent of block size. 0 (or glide off) means snap to target.
        let glide = ctx.portamento_on && ctx.portamento_time > 0.0;
        let glide_coeff = if glide {
            let dt = base_frames as f32 / base_rate;
            1.0 - (-dt / ctx.portamento_time).exp()
        } else {
            1.0
        };

        // ── Per-voice control-rate resolution (block start) ──
        let mut pw1 = [0.5f32; N];
        let mut pw2 = [0.5f32; N];
        for v in 0..N {
            let srcs = self.mod_sources(v, ctx, self.env1[v].level, self.env2[v].level);
            let pitch_mod = ctx.matrix.dest(ModDest::Pitch, &srcs);
            let cutoff_mod = ctx.matrix.dest(ModDest::Cutoff, &srcs);
            let pwm_mod = ctx.matrix.dest(ModDest::Pwm, &srcs);

            // Portamento: glide each channel's pitch toward its target note. A
            // freshly triggered channel snaps to target when glide is off, the
            // time is 0, or it has no previous pitch (its first note); otherwise
            // it ramps from where it was, giving JP-8 polyphonic glide per voice.
            let target = self.note[v] as f32;
            if self.trigger_pending[v] {
                if !glide || !self.glide_valid[v] {
                    self.glide_semi[v] = target;
                }
                self.glide_valid[v] = true;
            }
            self.glide_semi[v] += glide_coeff * (target - self.glide_semi[v]);
            let nf = self.glide_semi[v];
            let detune = self.detune_cents[v] * 0.01; // cents → semitones (Unison)
            let s1 = ctx.base_semis + nf + ctx.osc1_semi + pitch_mod + detune;
            let s2 = ctx.base_semis + nf + ctx.osc2_semi + pitch_mod + detune;
            self.osc1.inc[v] = note_to_hz(s1) / ctx.os_sample_rate;
            self.osc2.inc[v] = note_to_hz(s2) / ctx.os_sample_rate;
            pw1[v] = (ctx.osc1_pw + pwm_mod).clamp(0.05, 0.95);
            pw2[v] = (ctx.osc2_pw + pwm_mod).clamp(0.05, 0.95);

            let cutoff_hz = ctx.cutoff * fast_exp2(cutoff_mod / 12.0);
            self.ladder.set_coeffs(
                v,
                LadderCoeffs::new(
                    cutoff_hz,
                    ctx.os_sample_rate,
                    ctx.resonance,
                    ctx.drive,
                    ctx.variant,
                ),
            );
        }

        // Pre-VCF high-pass. Cutoff is global (not a mod destination), so the
        // coefficient is computed once and broadcast. At the default low cutoff
        // it's near-transparent, so bypass it entirely and feed the mixer
        // straight into the ladder (the common case pays nothing).
        let hpf_active = ctx.hpf_cutoff > HPF_OFF_HZ;
        if hpf_active {
            self.hpf.set_cutoff_all(ctx.hpf_cutoff, ctx.os_sample_rate);
        }
        // Ramp the ladder coefficients across this block's `base_frames * os`
        // samples so block-rate cutoff/LFO/envelope steps become a smooth
        // piecewise-linear coefficient trajectory (no zipper / staircase).
        self.ladder.prepare_ramp(base_frames * os);

        let mut trig = [false; N];
        trig.iter_mut()
            .zip(self.trigger_pending.iter_mut())
            .for_each(|(t, p)| *t = std::mem::take(p));

        // Scratch lane buffers.
        let mut o1 = [0.0f32; N];
        let mut o2 = [0.0f32; N];
        let mut nz = [0.0f32; N];
        let mut mix = [0.0f32; N];
        let mut hp = [0.0f32; N];
        let mut filt = [0.0f32; N];
        let mut amp = [0.0f32; N];

        for base_i in 0..base_frames {
            // Envelopes + amp (base rate, scalar; gated to 0 for inactive voices).
            for v in 0..N {
                let t = trig[v] && base_i == 0;
                let e1 = self.env1[v].tick(t, self.gate[v]);
                let e2 = self.env2[v].tick(t, self.gate[v]);
                amp[v] = if self.active[v] {
                    let srcs = self.mod_sources(v, ctx, e1, e2);
                    ctx.matrix.dest(ModDest::Amp, &srcs).max(0.0)
                } else {
                    0.0
                };
            }

            let frame = base_i * os;
            for k in 0..os {
                // Coupled osc2→osc1 path when sync is engaged or cross-mod depth
                // is non-zero; otherwise the independent, vectorised fast path —
                // no cost for plain patches.
                if ctx.sync || ctx.cross_mod != 0.0 {
                    self.osc1.process_pair(
                        &mut self.osc2,
                        ctx.sync,
                        ctx.cross_mod,
                        ctx.osc1_wave,
                        ctx.osc2_wave,
                        &pw1,
                        &pw2,
                        &mut o1,
                        &mut o2,
                    );
                } else {
                    self.osc1.process(ctx.osc1_wave, &pw1, &mut o1);
                    self.osc2.process(ctx.osc2_wave, &pw2, &mut o2);
                }
                self.noise.process(ctx.noise_color, &mut nz);
                for v in 0..N {
                    mix[v] =
                        o1[v] * ctx.osc1_level + o2[v] * ctx.osc2_level + nz[v] * ctx.noise_level;
                }
                // Source Mixer → HPF → VCF → VCA (JP-8 topology). HPF bypassed
                // when disengaged (default), feeding the mix straight to the VCF.
                let ladder_in = if hpf_active {
                    self.hpf.process(&mix, &mut hp);
                    &hp
                } else {
                    &mix
                };
                self.ladder.process(ladder_in, &mut filt);
                let mut sum = 0.0;
                for v in 0..N {
                    sum += filt[v] * amp[v];
                }
                out[frame + k] += sum * self.level_comp;
            }

            // Advance the per-voice LFO fade-in one base frame (held at 1).
            self.lfo_fade.advance(lfo_gain_inc);

            // Free voices whose envelopes have fully released.
            for v in 0..N {
                if self.active[v]
                    && !self.gate[v]
                    && self.env1[v].is_idle()
                    && self.env2[v].is_idle()
                {
                    self.active[v] = false;
                }
            }
        }
    }
}

/// Key-follow source value: octaves relative to middle C (note 60).
#[inline]
fn key_follow(note: u8) -> f32 {
    (note as f32 - 60.0) / 12.0
}

/// Fixed symmetric detune weight for unison channel `v`, in `[-1, 1]` across the
/// layer's channels (scaled by the `UnisonDetune` cents param). Per-channel and
/// constant — deterministic, not random per note — so it is testable.
#[inline]
fn unison_spread(v: usize) -> f32 {
    if N <= 1 {
        0.0
    } else {
        (v as f32 / (N - 1) as f32) * 2.0 - 1.0
    }
}
