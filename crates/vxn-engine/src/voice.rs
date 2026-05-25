//! Structure-of-arrays voice bank: all 16 voices processed together so the
//! oscillator/filter/noise hot path vectorises across voices (see
//! `vxn_dsp::poly`). Envelopes stay scalar (one [`AdsrCore`] per voice) and
//! tick at the base rate; the oscillators and ladder run at the oversampled
//! rate.
//!
//! Modulation model (fixed routes — ADR 0004 §4): each of the Pitch / PWM /
//! Cutoff channels picks one LFO source ({Off/LFO1/LFO2}) and one envelope
//! source ({Off/Env1/Env2}), scaled by per-channel depths; the common pitch
//! channel moves both oscillators (vibrato-scaled), a separate wide route moves
//! osc2 only (sync sweeps). The VCA is hardwired to Env2. Cutoff also takes
//! velocity, an optional 1-oct/oct key-track, and the mod-wheel panel
//! contributions. Pitch/cutoff/PWM are resolved once per control block; the amp
//! (Env2) is evaluated per base frame.

use vxn_dsp::{
    AdsrCore, AdsrShape, CHANNELS_PER_LAYER, CONTROL_BLOCK, LadderCoeffs, LadderVariant, LfoCore,
    LfoShape, NoiseColor, PolyHpf, PolyLadder, PolyNoise, PolyOscillator, Waveform, fast_exp2,
    note_to_hz, poly_ring_mod,
};

use crate::params::{AssignMode, EnvSel, LfoSel};

/// One [`VoiceBank`] is a single layer: its channels render together as a
/// homogeneous group (ADR 0003 §10).
const N: usize = CHANNELS_PER_LAYER;

/// HPF cutoff at or below this (Hz) is treated as "off" and bypassed. Matches
/// the `HpfCutoff` param minimum (its default, ≈ fully open).
const HPF_OFF_HZ: f32 = 20.0;

/// Fixed ring-modulator diode drive (dB). No panel knob in v1 (ADR 0004 panel
/// list leaves it out); the operating point sits in the quasi-linear region.
const RING_DRIVE_DB: f32 = 1.0;

/// Per-voice LFO 1 retrigger policy at a note-on (E005 / 0018): the shape (for
/// the zero-crossing restart) and whether the phase free-runs instead.
#[derive(Clone, Copy)]
pub struct Lfo1Trigger {
    pub shape: LfoShape,
    pub free_run: bool,
}

/// Per-voice two-stage onset for the per-voice LFO 1 (E005 / 0018): after a
/// voice's note-on, its LFO 1 depth is held at zero for `delay` seconds, then
/// ramps 0→1 over `fade` seconds. `delay = fade = 0` pins depth to full
/// immediately, reproducing the undelayed path. `t` is seconds since note-on,
/// capped so it stays finite over long-held notes; untriggered voices sit at
/// `f32::MAX` (settled at full depth).
#[derive(Clone)]
struct Lfo1Onset {
    t: [f32; N],
}

impl Lfo1Onset {
    fn new() -> Self {
        Self { t: [f32::MAX; N] }
    }

    fn reset(&mut self) {
        self.t = [f32::MAX; N];
    }

    /// Restart voice `v`'s onset from note-on.
    #[inline]
    fn retrigger(&mut self, v: usize) {
        self.t[v] = 0.0;
    }

    /// Depth gain for voice `v` given the current `delay` / `fade` (s).
    #[inline]
    fn gain(&self, v: usize, delay: f32, fade: f32) -> f32 {
        let t = self.t[v];
        if t < delay {
            0.0
        } else if fade <= 0.0 {
            1.0
        } else {
            ((t - delay) / fade).min(1.0)
        }
    }

    /// Advance every voice by `dt` seconds, capped at `cap` (= delay + fade) so
    /// `t` stays finite once a voice has fully faded in.
    #[inline]
    fn advance(&mut self, dt: f32, cap: f32) {
        for t in &mut self.t {
            if *t < cap {
                *t = (*t + dt).min(cap);
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
    /// Ring-modulator (osc1×osc2) mix level (0021). 0 = the cheap no-op path.
    pub ring_level: f32,
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
    /// LFO 1 is per-voice (E005 / 0018): the bank ticks its own phases, so the
    /// block carries LFO 1's shape, resolved rate (Hz, post host-sync) and the
    /// two-stage onset times rather than a single sampled value.
    pub lfo1_shape: LfoShape,
    pub lfo1_rate_hz: f32,
    /// LFO 1 onset: hold modulation at zero for `lfo1_delay_time` s, then ramp
    /// over `lfo1_fade` s. Both 0 = full depth immediately.
    pub lfo1_delay_time: f32,
    pub lfo1_fade: f32,
    /// Global LFO 2 sampled value this block (one instrument-wide LFO, sampled
    /// once and broadcast to both layers — E005 / 0019). Constant depth, no delay.
    pub lfo2_val: f32,
    /// Hard sync on (`CrossModType::Sync`): osc2 (slave) phase resets each osc1
    /// (master) cycle. Off keeps the independent, vectorised osc fast path.
    pub sync: bool,
    /// Through-zero phase-mod index (`CrossModType::Pm` ? amount : 0). 0 = off.
    /// Engages the coupled osc path; mutually exclusive with `sync` at the engine.
    pub pm_index: f32,
    /// Portamento (pitch glide) enabled for this layer.
    pub portamento_on: bool,
    /// Portamento glide time (s); 0 = instant. Glide is per channel, resolved at
    /// control-block rate so it feeds osc pitch, sync and PM consistently.
    pub portamento_time: f32,
    // ── Fixed modulation routes (ADR 0004 §4). Depths are pre-smoothed; the
    //    `*_extra` terms fold in the once-per-block global contributions
    //    (pitch-wheel for pitch, mod-wheel panel elsewhere). ──
    /// Common pitch channel (vibrato-scaled — moves both oscillators).
    pub pitch_lfo_sel: LfoSel,
    pub pitch_lfo_depth: f32,
    pub pitch_env_sel: EnvSel,
    pub pitch_env_depth: f32,
    /// Pitch-wheel contribution (bend × wheel depth, semitones), both oscillators.
    pub pitch_extra: f32,
    /// PWM channel.
    pub pwm_lfo_sel: LfoSel,
    pub pwm_lfo_depth: f32,
    pub pwm_env_sel: EnvSel,
    pub pwm_env_depth: f32,
    /// Mod-wheel → PWM contribution (fraction).
    pub pwm_extra: f32,
    /// Cutoff channel (semitones of cutoff).
    pub cutoff_lfo_sel: LfoSel,
    pub cutoff_lfo_depth: f32,
    pub cutoff_env_sel: EnvSel,
    pub cutoff_env_depth: f32,
    pub cutoff_vel_depth: f32,
    /// Mod-wheel → cutoff contribution (semitones).
    pub cutoff_extra: f32,
    /// Filter key-track: when on, cutoff shifts exactly 1 octave per key octave
    /// above C0 (12 st cutoff per 12 st key).
    pub filter_key_track: bool,
    /// Wide osc-2 pitch channel (octave range — moves osc2 only).
    pub osc2_pitch_env_sel: EnvSel,
    pub osc2_pitch_env_depth: f32,
    /// Mod-wheel → osc2 pitch contribution (semitones).
    pub osc2_pitch_extra: f32,
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
    /// Whether the last note was triggered in Unison mode. Drives the gentler
    /// unison glide scaling (the whole detuned stack slides at once, so the same
    /// knob position wants a far subtler time) — set per `note_on`.
    unison: bool,
    /// Per-channel glided pitch (MIDI note as f32). With portamento it ramps
    /// toward the target note at control-block rate; without, it tracks the note.
    glide_semi: [f32; N],
    /// Whether a channel has a previous pitch to glide *from*. False until its
    /// first note, so the first note never sweeps up from zero.
    glide_valid: [bool; N],
    /// Per-voice LFO 1 (E005 / 0018): one phase per channel, retriggered at that
    /// channel's note-on, ticked once per control block.
    lfo1: [LfoCore; N],
    /// Per-voice LFO 1 two-stage onset (delay → fade).
    lfo1_onset: Lfo1Onset,
    /// Seed base for the per-channel LFO 1 cores; kept so they can be rebuilt at
    /// the new control rate on a sample-rate change.
    lfo1_seed: u64,
}

/// Decorrelated per-channel LFO 1 seed from the layer's base seed.
#[inline]
fn lfo1_seed(base: u64, ch: usize) -> u64 {
    base.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((ch as u64 + 1).wrapping_mul(0x632B_E5A6))
}

impl VoiceBank {
    /// `noise_seed` differs per layer so two layers' noise generators are
    /// decorrelated (no comb artefacts when two similar patches sum).
    pub fn new(sample_rate: f32, noise_seed: u64) -> Self {
        // The LFO ticks once per control block, so its cores run at the control
        // rate (sr / CONTROL_BLOCK), matching the old per-layer LFO.
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
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
            unison: false,
            glide_semi: [0.0; N],
            glide_valid: [false; N],
            lfo1: std::array::from_fn(|i| LfoCore::new(control_rate, lfo1_seed(noise_seed, i))),
            lfo1_onset: Lfo1Onset::new(),
            lfo1_seed: noise_seed,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.env1 = std::array::from_fn(|_| AdsrCore::new(sample_rate));
        self.env2 = std::array::from_fn(|_| AdsrCore::new(sample_rate));
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
        let seed = self.lfo1_seed;
        self.lfo1 = std::array::from_fn(|i| LfoCore::new(control_rate, lfo1_seed(seed, i)));
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
        self.unison = false;
        self.glide_semi = [0.0; N];
        self.glide_valid = [false; N];
        for lfo in &mut self.lfo1 {
            lfo.reset();
        }
        self.lfo1_onset.reset();
    }

    pub fn active_count(&self) -> usize {
        self.active.iter().filter(|&&a| a).count()
    }

    /// Channel `v`'s per-voice LFO 1 phase (E005 / 0018). Exposed for tests to
    /// observe per-voice retrigger / free-run behaviour.
    #[cfg(test)]
    pub(crate) fn lfo1_phase(&self, v: usize) -> f32 {
        self.lfo1[v].phase()
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
        lfo1: Lfo1Trigger,
    ) {
        match mode {
            AssignMode::Poly => {
                let v = self.allocate(note);
                // DCO behaviour: phase resets to zero on every Poly note.
                self.trigger(v, note, velocity, alloc_tick, 0.0, 0.0, lfo1);
                self.level_comp = 1.0;
                self.unison = false;
            }
            AssignMode::Unison => {
                // Last-note priority: every channel retriggers to the new note
                // (the prior note is not stacked). Per-channel detune fans the 8
                // copies out, and a spread of start phases (rather than the Poly
                // phase-0 reset) decorrelates the detuned copies' beating so they
                // don't comb into synchronised nulls and thin the sound out.
                for v in 0..N {
                    self.trigger(
                        v,
                        note,
                        velocity,
                        alloc_tick,
                        unison_spread(v) * unison_detune,
                        unison_phase(v),
                        lfo1,
                    );
                }
                self.level_comp = 1.0 / (N as f32).sqrt();
                self.unison = true;
            }
        }
    }

    /// Trigger a specific channel: the lowest level of the assign seam. Poly hits
    /// one channel, Unison hits all; both route through here so per-channel state
    /// (gate, detune, phase reset) is set in exactly one place.
    fn trigger(
        &mut self,
        v: usize,
        note: u8,
        velocity: f32,
        alloc_tick: u64,
        detune_cents: f32,
        start_phase: f32,
        lfo1: Lfo1Trigger,
    ) {
        self.note[v] = note;
        self.velocity[v] = velocity;
        self.gate[v] = true;
        self.active[v] = true;
        self.trigger_pending[v] = true;
        self.alloc_tick[v] = alloc_tick;
        self.detune_cents[v] = detune_cents;
        // Per-voice LFO 1: restart its onset, and (unless free-running) retrigger
        // its phase to the shape's zero crossing so modulation eases out of zero.
        self.lfo1_onset.retrigger(v);
        if !lfo1.free_run {
            self.lfo1[v].retrigger(lfo1.shape);
        }
        self.osc1.reset(v);
        self.osc2.reset(v);
        // Offset the (otherwise zeroed) start phase per channel. Same offset for
        // both oscillators so a voice's osc1/osc2 relationship is preserved; the
        // offset only decorrelates voices from each other (Unison). Poly passes 0.
        self.osc1.phase[v] = start_phase;
        self.osc2.phase[v] = start_phase;
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

    /// Pick a voice: re-use one already playing this note, else the free voice
    /// whose last pitch sits nearest the new note, else steal the oldest.
    ///
    /// Choosing the *nearest* free voice (by its glide source `glide_semi`, the
    /// pitch it would sweep from) keeps Poly glide musical: a new note slides the
    /// shortest distance, and a free voice already at that pitch snaps cleanly
    /// instead of some far-off voice sweeping across the keyboard.
    fn allocate(&self, note: u8) -> usize {
        if let Some(v) = (0..N).find(|&v| self.active[v] && self.note[v] == note) {
            return v;
        }
        if let Some(v) = (0..N)
            .filter(|&v| !self.active[v])
            .min_by(|&a, &b| {
                let target = note as f32;
                (self.glide_semi[a] - target)
                    .abs()
                    .total_cmp(&(self.glide_semi[b] - target).abs())
            })
        {
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

    /// Render one control block into the oversampled mono buffer `out`
    /// (length = `base_frames * ctx.os`), accumulating all voices.
    pub fn render_block(&mut self, out: &mut [f32], ctx: &BlockCtx) {
        let os = ctx.os;
        let base_frames = out.len() / os;
        let base_rate = ctx.os_sample_rate / os as f32;

        // Per-voice LFO 1: tick each channel's phase once for this block (held
        // across the block's frames, like the old per-layer LFO). The onset gain
        // (delay → fade) is applied at each read site, since it ramps per frame.
        let mut lfo1_raw = [0.0f32; N];
        for (lfo, raw) in self.lfo1.iter_mut().zip(lfo1_raw.iter_mut()) {
            lfo.set_rate(ctx.lfo1_rate_hz);
            *raw = lfo.next(ctx.lfo1_shape);
        }
        let onset_cap = ctx.lfo1_delay_time + ctx.lfo1_fade;
        let onset_dt = 1.0 / base_rate;

        // Portamento glide coefficient for this block (one-pole toward the target
        // note). `dt` is the block's wall-clock duration, so the glide rate is
        // independent of block size. 0 (or glide off) means snap to target.
        let glide = ctx.portamento_on && ctx.portamento_time > 0.0;
        // The whole detuned Unison stack glides together, so the same knob
        // position reads far stronger than one Poly voice — scale the time right
        // down so Unison glide is a subtle scoop, not an audible stack slide.
        let glide_time = if self.unison {
            ctx.portamento_time * UNISON_GLIDE_SCALE
        } else {
            ctx.portamento_time
        };
        let glide_coeff = if glide {
            let dt = base_frames as f32 / base_rate;
            1.0 - (-dt / glide_time).exp()
        } else {
            1.0
        };

        // ── Per-voice control-rate resolution (block start) ──
        let mut pw1 = [0.5f32; N];
        let mut pw2 = [0.5f32; N];
        for v in 0..N {
            let e1 = self.env1[v].level;
            let e2 = self.env2[v].level;
            let lfo1 = lfo1_raw[v] * self.lfo1_onset.gain(v, ctx.lfo1_delay_time, ctx.lfo1_fade);

            // Fixed-route resolution: each channel sums its selected LFO × depth,
            // its selected env × depth, and the channel's extra (ADR 0004 §4).
            let pitch_mod = lfo_src(ctx.pitch_lfo_sel, lfo1, ctx.lfo2_val) * ctx.pitch_lfo_depth
                + env_src(ctx.pitch_env_sel, e1, e2) * ctx.pitch_env_depth
                + ctx.pitch_extra;
            // Wide osc-2 pitch (sync sweeps): osc2 only, added on top of common pitch.
            let osc2_pitch_mod = env_src(ctx.osc2_pitch_env_sel, e1, e2) * ctx.osc2_pitch_env_depth
                + ctx.osc2_pitch_extra;
            let pwm_mod = lfo_src(ctx.pwm_lfo_sel, lfo1, ctx.lfo2_val) * ctx.pwm_lfo_depth
                + env_src(ctx.pwm_env_sel, e1, e2) * ctx.pwm_env_depth
                + ctx.pwm_extra;
            let key_track = if ctx.filter_key_track {
                // 1 octave of cutoff per octave of key above C0 (note 12).
                self.note[v] as f32 - 12.0
            } else {
                0.0
            };
            let cutoff_mod = lfo_src(ctx.cutoff_lfo_sel, lfo1, ctx.lfo2_val) * ctx.cutoff_lfo_depth
                + env_src(ctx.cutoff_env_sel, e1, e2) * ctx.cutoff_env_depth
                + self.velocity[v] * ctx.cutoff_vel_depth
                + key_track
                + ctx.cutoff_extra;

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
            let s2 = ctx.base_semis + nf + ctx.osc2_semi + pitch_mod + osc2_pitch_mod + detune;
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
        let mut ring = [0.0f32; N];
        let mut mix = [0.0f32; N];
        let mut hp = [0.0f32; N];
        let mut filt = [0.0f32; N];
        let mut amp = [0.0f32; N];

        // Ring modulator (0021): osc1×osc2 through the Parker diode bridge, mixed
        // by `ring_level`. Zero level skips the diode maths entirely (fast path).
        let ring_on = ctx.ring_level != 0.0;
        let ring_gain = 10.0f32.powf(RING_DRIVE_DB / 20.0);

        for base_i in 0..base_frames {
            // Envelopes + amp (base rate, scalar; gated to 0 for inactive voices).
            // The VCA is hardwired to Env2 (ADR 0004 §4); Env1 still ticks so it
            // can feed the modulation routes from its stored level.
            for v in 0..N {
                let t = trig[v] && base_i == 0;
                let _e1 = self.env1[v].tick(t, self.gate[v]);
                let e2 = self.env2[v].tick(t, self.gate[v]);
                amp[v] = if self.active[v] { e2.max(0.0) } else { 0.0 };
            }

            let frame = base_i * os;
            for k in 0..os {
                // Coupled osc2→osc1 path when sync is engaged or the PM index is
                // non-zero; otherwise the independent, vectorised fast path —
                // no cost for plain patches. Sync and PM are mutually exclusive at
                // the engine (`CrossModType`), so each picks its specialised kernel
                // and pays for only its own work (the combined `process_pair` is
                // kept as the reference oracle).
                if ctx.sync {
                    self.osc1.process_sync(
                        &mut self.osc2,
                        ctx.osc1_wave,
                        ctx.osc2_wave,
                        &pw1,
                        &pw2,
                        &mut o1,
                        &mut o2,
                    );
                } else if ctx.pm_index != 0.0 {
                    self.osc1.process_pm(
                        &mut self.osc2,
                        ctx.pm_index,
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
                // Ring contribution (osc1×osc2), summed in alongside the oscs.
                if ring_on {
                    poly_ring_mod(&o1, &o2, ring_gain, &mut ring);
                    for v in 0..N {
                        mix[v] += ring[v] * ctx.ring_level;
                    }
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

            // Advance the per-voice LFO 1 onset one base frame.
            self.lfo1_onset.advance(onset_dt, onset_cap);

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

/// Resolve a channel's LFO source selector to a value (per-voice LFO 1 is
/// onset-scaled by the caller; LFO 2 is the global broadcast value).
#[inline]
fn lfo_src(sel: LfoSel, lfo1: f32, lfo2: f32) -> f32 {
    match sel {
        LfoSel::Off => 0.0,
        LfoSel::Lfo1 => lfo1,
        LfoSel::Lfo2 => lfo2,
    }
}

/// Resolve a channel's envelope source selector to a value.
#[inline]
fn env_src(sel: EnvSel, env1: f32, env2: f32) -> f32 {
    match sel {
        EnvSel::Off => 0.0,
        EnvSel::Env1 => env1,
        EnvSel::Env2 => env2,
    }
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

/// Fixed Unison start phase for channel `v`, spread across the first **half**
/// cycle `[0, 0.5]`. Offsetting the start phases (rather than the Poly phase-0
/// reset for all) staggers when each detuned ± pair reaches its beat trough, so
/// they no longer comb into one synchronised null that thins the sound. A half
/// cycle (not the full circle) is deliberate: a full even spread sums to zero for
/// coherent copies (detune 0), gutting the level, whereas a half-cycle spread
/// keeps a strong coherent sum while still decorrelating the beating. Deterministic
/// per channel, so the unison sum is reproducible / testable.
#[inline]
fn unison_phase(v: usize) -> f32 {
    if N <= 1 {
        0.0
    } else {
        0.5 * v as f32 / (N - 1) as f32
    }
}

/// Unison glide-time scaling: the detuned stack slides together and reads far
/// stronger than one Poly voice, so its effective portamento time is cut to this
/// fraction of the knob value for a subtle scoop rather than an audible slide.
const UNISON_GLIDE_SCALE: f32 = 0.15;
