//! Structure-of-arrays voice bank: all 16 voices processed together so the
//! oscillator/filter hot path vectorises across voices (see
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
    AdsrCore, AdsrShape, AdsrStage, CHANNELS_PER_LAYER, CONTROL_BLOCK, LadderCoeffs, LadderVariant,
    LfoCore, LfoShape, PolyHpf, PolyLadder, PolyOscillator, Waveform,
    fast_exp2, note_to_hz, poly_ring_mod,
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
    /// Oversampling factor (1, 2, 4 or 8).
    pub os: usize,
    pub osc1_wave: Waveform,
    pub osc2_wave: Waveform,
    pub osc1_level: f32,
    pub osc2_level: f32,
    /// Ring-modulator (osc1×osc2) mix level (0021). 0 = the cheap no-op path.
    pub ring_level: f32,
    pub osc1_pw: f32,
    pub osc2_pw: f32,
    pub osc1_semi: f32,
    pub osc2_semi: f32,
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
    /// Portamento glide time (s); 0 = off/instant (no separate on/off). Per
    /// channel, resolved at
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
    /// Cutoff channel (semitones of cutoff) — fixed sources (E006): each of LFO 1,
    /// LFO 2, Env 1 and velocity has its own depth; no source selectors.
    pub cutoff_lfo1_depth: f32,
    pub cutoff_lfo2_depth: f32,
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
    /// `rng_seed` differs per layer so the two layers' S&H LFO PRNGs are
    /// decorrelated (no shared random sequence when two similar patches sum).
    pub fn new(sample_rate: f32, rng_seed: u64) -> Self {
        // The LFO ticks once per control block, so its cores run at the control
        // rate (sr / CONTROL_BLOCK), matching the old per-layer LFO.
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
        Self {
            osc1: PolyOscillator::new(),
            osc2: PolyOscillator::new(),
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
            lfo1: std::array::from_fn(|i| LfoCore::new(control_rate, lfo1_seed(rng_seed, i))),
            lfo1_onset: Lfo1Onset::new(),
            lfo1_seed: rng_seed,
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
        // Decide *which* channels and their detune/phase purely from bookkeeping
        // (`plan`), then apply the DSP effect (`trigger`) per assignment. The
        // borrow in `alloc_view` ends when `plan` returns its owned result, so the
        // mutating `trigger` calls below are free to touch the same arrays.
        let plan = plan(mode, note, unison_detune, self.alloc_view());
        for a in plan.iter() {
            self.trigger(
                a.channel,
                note,
                velocity,
                alloc_tick,
                a.detune_cents,
                a.start_phase,
                lfo1,
            );
        }
        self.level_comp = plan.level_comp;
        self.unison = plan.unison;
    }

    /// Trigger a specific channel: the lowest level of the assign seam. Poly hits
    /// one channel, Unison hits all; both route through here so per-channel state
    /// (gate, detune, phase reset) is set in exactly one place.
    #[allow(clippy::too_many_arguments)] // one coupled per-trigger param set, single caller
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

    /// Read-only snapshot of the bookkeeping the allocation policy reads. Borrows
    /// the relevant arrays so [`plan`] can run without touching DSP state.
    #[inline]
    fn alloc_view(&self) -> AllocView<'_> {
        AllocView {
            active: &self.active,
            note: &self.note,
            glide_semi: &self.glide_semi,
            alloc_tick: &self.alloc_tick,
        }
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
        // note); see `block_glide`. The glide is off / snaps when disabled.
        let (glide, glide_coeff) =
            block_glide(ctx.portamento_time, self.unison, base_frames, base_rate);

        // ── Per-voice control-rate resolution (block start) ──
        let mut pw1 = [0.5f32; N];
        let mut pw2 = [0.5f32; N];
        for v in 0..N {
            // LFO 1's onset gain ramps per frame, so it's applied here (reading the
            // per-voice onset state) before handing a plain value to `resolve_mod`.
            let lfo1 = lfo1_raw[v] * self.lfo1_onset.gain(v, ctx.lfo1_delay_time, ctx.lfo1_fade);
            let m = resolve_mod(
                ctx,
                &ModSources {
                    e1: self.env1[v].level,
                    e2: self.env2[v].level,
                    lfo1,
                    lfo2: ctx.lfo2_val,
                    velocity: self.velocity[v],
                    note: self.note[v],
                },
            );

            // Portamento: glide each channel's pitch toward its target note. A
            // freshly triggered channel snaps to target when glide is off, the
            // time is 0, or it has no previous pitch (its first note); otherwise
            // it ramps from where it was, giving JP-8 polyphonic glide per voice.
            // The glide is a stateful recurrence (and the osc/filter coefficient
            // writes below are DSP application), so they stay inline; only the
            // pure route maths is lifted into `resolve_mod`.
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
            let s1 = ctx.base_semis + nf + ctx.osc1_semi + m.pitch_mod + detune;
            let s2 = ctx.base_semis + nf + ctx.osc2_semi + m.pitch_mod + m.osc2_pitch_mod + detune;
            self.osc1.inc[v] = note_to_hz(s1) / ctx.os_sample_rate;
            self.osc2.inc[v] = note_to_hz(s2) / ctx.os_sample_rate;
            pw1[v] = (ctx.osc1_pw + m.pwm_mod).clamp(0.05, 0.95);
            pw2[v] = (ctx.osc2_pw + m.pwm_mod).clamp(0.05, 0.95);

            let cutoff_hz = ctx.cutoff * fast_exp2(m.cutoff_mod / 12.0);
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
        let mut ring = [0.0f32; N];
        let mut mix = [0.0f32; N];
        let mut hp = [0.0f32; N];
        let mut filt = [0.0f32; N];
        let mut amp = [0.0f32; N];

        // Ring modulator (0021): osc1×osc2 through the Parker diode bridge, mixed
        // by `ring_level`. Zero level skips the diode maths entirely (fast path).
        let ring_on = ctx.ring_level != 0.0;
        let ring_gain = 10.0f32.powf(RING_DRIVE_DB / 20.0);

        // Envelope block-skip (see `envelopes_static`): when nothing triggers and
        // every active voice holds both envelopes in Sustain, the env levels are
        // constant, so `amp` is computed once and the per-frame tick + free-check
        // are skipped. Otherwise the per-frame path runs.
        let env_static =
            envelopes_static(&trig, &self.active, &self.gate, &self.env1, &self.env2);
        if env_static {
            for (v, amp_v) in amp.iter_mut().enumerate() {
                *amp_v = if self.active[v] {
                    self.env2[v].level.max(0.0)
                } else {
                    0.0
                };
            }
        }

        for base_i in 0..base_frames {
            // Envelopes + amp (base rate, scalar; gated to 0 for inactive voices).
            // The VCA is hardwired to Env2 (ADR 0004 §4); Env1 still ticks so it
            // can feed the modulation routes from its stored level. Skipped when
            // the block is envelope-static (see `env_static` above).
            if !env_static {
                for v in 0..N {
                    let t = trig[v] && base_i == 0;
                    let _e1 = self.env1[v].tick(t, self.gate[v]);
                    let e2 = self.env2[v].tick(t, self.gate[v]);
                    amp[v] = if self.active[v] { e2.max(0.0) } else { 0.0 };
                }
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
                for v in 0..N {
                    mix[v] = o1[v] * ctx.osc1_level + o2[v] * ctx.osc2_level;
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

            // Free voices whose envelopes have fully released. Skipped when the
            // block is envelope-static: every active voice is Sustain/gate-high
            // there, so none can be idle-and-releasing.
            if !env_static {
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

// ── Per-voice modulation resolution ──────────────────────────────────────────
//
// The fixed-route maths (ADR 0004 §4): sum each channel's selected LFO × depth,
// selected env × depth, velocity / key-track / wheel extras into the four mod
// destinations. Pure — sources in, offsets out, no `self`, no DSP, no sample
// rate — so the routing table (selector → source, depth sign, key-track curve)
// is unit-testable in isolation, like the allocation policy. `render_block`
// keeps the stateful apply (glide recurrence, osc/filter coefficient writes).

/// Read-only modulation sources for one channel at block start. Everything
/// `resolve_mod` reads; constructed in tests directly from plain values.
struct ModSources {
    /// Env 1 level (feeds routes; the VCA is hardwired to env 2 elsewhere).
    e1: f32,
    /// Env 2 level.
    e2: f32,
    /// Per-voice LFO 1, already onset-scaled by the caller.
    lfo1: f32,
    /// Global LFO 2 broadcast value.
    lfo2: f32,
    /// Note velocity (cutoff route).
    velocity: f32,
    /// MIDI note, for the filter key-track curve.
    note: u8,
}

/// Resolved per-channel mod offsets: pitch / osc2-pitch / cutoff in semitones,
/// pwm as a pulse-width fraction.
struct ModOut {
    pitch_mod: f32,
    osc2_pitch_mod: f32,
    pwm_mod: f32,
    cutoff_mod: f32,
}

/// Fixed-route resolution for one channel (ADR 0004 §4). Pure: no `self`, no
/// state mutation, no sample rate.
#[inline]
fn resolve_mod(ctx: &BlockCtx, s: &ModSources) -> ModOut {
    // 1 octave of cutoff per octave of key relative to C4 (note 60): cutoff is
    // unchanged at C4, rises above it, falls below it.
    let key_track = if ctx.filter_key_track {
        s.note as f32 - 60.0
    } else {
        0.0
    };
    ModOut {
        pitch_mod: lfo_src(ctx.pitch_lfo_sel, s.lfo1, s.lfo2) * ctx.pitch_lfo_depth
            + env_src(ctx.pitch_env_sel, s.e1, s.e2) * ctx.pitch_env_depth
            + ctx.pitch_extra,
        // Wide osc-2 pitch (sync sweeps): osc2 only, added on top of common pitch.
        osc2_pitch_mod: env_src(ctx.osc2_pitch_env_sel, s.e1, s.e2) * ctx.osc2_pitch_env_depth
            + ctx.osc2_pitch_extra,
        pwm_mod: lfo_src(ctx.pwm_lfo_sel, s.lfo1, s.lfo2) * ctx.pwm_lfo_depth
            + env_src(ctx.pwm_env_sel, s.e1, s.e2) * ctx.pwm_env_depth
            + ctx.pwm_extra,
        // Fixed cutoff sources (E006): LFO 1, LFO 2 and Env 1 each by their own
        // depth, plus velocity, key-track and the mod-wheel `extra`.
        cutoff_mod: s.lfo1 * ctx.cutoff_lfo1_depth
            + s.lfo2 * ctx.cutoff_lfo2_depth
            + s.e1 * ctx.cutoff_env_depth
            + s.velocity * ctx.cutoff_vel_depth
            + key_track
            + ctx.cutoff_extra,
    }
}

/// Portamento glide for this block: `(active, coeff)`. The one-pole coefficient
/// toward the target note is derived from the block's wall-clock duration
/// (`base_frames / base_rate`), so the glide rate is independent of block size.
/// `unison` scales the time down — the whole detuned stack slides together, so
/// the same knob position reads far stronger than one Poly voice, and a subtle
/// scoop is wanted rather than an audible stack slide. Time 0 is glide off and
/// returns `(false, 1.0)`: the caller snaps straight to the target. Pure.
#[inline]
fn block_glide(
    portamento_time: f32,
    unison: bool,
    base_frames: usize,
    base_rate: f32,
) -> (bool, f32) {
    if portamento_time <= 0.0 {
        return (false, 1.0);
    }
    let glide_time = if unison {
        portamento_time * UNISON_GLIDE_SCALE
    } else {
        portamento_time
    };
    let dt = base_frames as f32 / base_rate;
    (true, 1.0 - (-dt / glide_time).exp())
}

/// Envelope block-skip predicate: true when no voice triggers this block and
/// every active voice holds *both* envelopes in Sustain (gate high), so the env
/// levels are constant across the block and the per-frame tick + free-check can
/// be skipped. Bit-identical: a held Sustain tick is idempotent (`level =
/// sustain`), so 0 ticks and `os·n` ticks leave the same state, and no
/// Sustain/gate-high voice can free. Any trigger, or a voice mid attack / decay
/// / release, forces the per-frame path. Pure.
#[inline]
fn envelopes_static(
    trig: &[bool; N],
    active: &[bool; N],
    gate: &[bool; N],
    env1: &[AdsrCore; N],
    env2: &[AdsrCore; N],
) -> bool {
    trig.iter().all(|&t| !t)
        && (0..N).all(|v| {
            !active[v]
                || (gate[v]
                    && env1[v].stage == AdsrStage::Sustain
                    && env2[v].stage == AdsrStage::Sustain)
        })
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

// ── Voice-allocation policy ──────────────────────────────────────────────────
//
// Pure functions that decide *which* channels a note-on lands on and the
// per-channel detune / start-phase to stamp, given only the layer's bookkeeping.
// No oscillators, filters, envelopes or sample rate — so the policy (steal order,
// unison spread, future Solo/Twin modes) is unit-testable in isolation, and
// `note_on` is left to apply the DSP effect (`trigger`).

/// Read-only bookkeeping the allocation policy reads. Borrows the bank's arrays;
/// constructed in tests directly from plain arrays.
#[derive(Clone, Copy)]
struct AllocView<'a> {
    active: &'a [bool; N],
    note: &'a [u8; N],
    /// Per-channel glide source pitch — the pitch a free channel would sweep from
    /// (drives nearest-free choice for musical Poly glide).
    glide_semi: &'a [f32; N],
    /// Per-channel allocation tick — lowest is oldest, stolen first.
    alloc_tick: &'a [u64; N],
}

/// One channel assignment: which channel to trigger and the per-channel detune
/// (cents) / start phase to stamp on it. Pure data — `trigger` applies it.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Assign {
    channel: usize,
    detune_cents: f32,
    start_phase: f32,
}

/// The outcome of a note-on policy: up to `N` channel assignments plus the
/// derived level compensation and unison flag (both fall out of the assignment
/// count — `1/√k` for a `k`-channel stack, `unison` set whenever `k > 1`).
struct Plan {
    assigns: [Assign; N],
    len: usize,
    level_comp: f32,
    unison: bool,
}

impl Plan {
    /// Build from the first `len` assignments; derives `level_comp` / `unison`.
    fn new(assigns: [Assign; N], len: usize) -> Self {
        Self {
            assigns,
            len,
            level_comp: 1.0 / (len as f32).sqrt(),
            unison: len > 1,
        }
    }

    fn iter(&self) -> impl Iterator<Item = Assign> + '_ {
        self.assigns[..self.len].iter().copied()
    }
}

/// A channel index that can never match a real channel — "exclude nothing".
const NO_SKIP: usize = usize::MAX;

/// Pick one channel, skipping `skip`: re-use one already playing this note, else
/// the free channel whose glide source sits nearest the new note, else steal the
/// oldest.
///
/// Choosing the *nearest* free channel (by `glide_semi`, the pitch it would sweep
/// from) keeps glide musical: a new note slides the shortest distance, and a
/// free channel already at that pitch snaps cleanly instead of some far-off
/// channel sweeping across the keyboard. `skip` lets [`allocate_pair`] take a
/// second, distinct channel by the same priority.
fn allocate_excl(note: u8, st: AllocView, skip: usize) -> usize {
    if let Some(v) = (0..N).find(|&v| v != skip && st.active[v] && st.note[v] == note) {
        return v;
    }
    if let Some(v) = (0..N)
        .filter(|&v| v != skip && !st.active[v])
        .min_by(|&a, &b| {
            let target = note as f32;
            (st.glide_semi[a] - target)
                .abs()
                .total_cmp(&(st.glide_semi[b] - target).abs())
        })
    {
        return v;
    }
    (0..N)
        .filter(|&v| v != skip)
        .min_by_key(|&v| st.alloc_tick[v])
        .unwrap_or(0)
}

/// Single-channel allocation (Poly): the common case, excluding nothing.
#[inline]
fn allocate_one(note: u8, st: AllocView) -> usize {
    allocate_excl(note, st, NO_SKIP)
}

/// Two distinct channels for a Twin note: the top-priority channel, then the
/// next by the same rule. Reuses both channels of a Twin pair already on this
/// note; otherwise two nearest-free; otherwise the two oldest stolen.
fn allocate_pair(note: u8, st: AllocView) -> (usize, usize) {
    let a = allocate_excl(note, st, NO_SKIP);
    let b = allocate_excl(note, st, a);
    (a, b)
}

/// Monophonic allocation (Solo): keep sounding on the one already-active channel
/// (legato glide carries through its `glide_semi`); fall back to channel 0 when
/// nothing is playing.
#[inline]
fn allocate_mono(st: AllocView) -> usize {
    (0..N).find(|&v| st.active[v]).unwrap_or(0)
}

/// Twin pitch spread reuses the Unison extremes (±`UnisonDetune` cents); the two
/// channels sit at the opposite ends of that fan.
const TWIN_SPREAD: f32 = 1.0;
/// Twin start-phase offset for the second channel — a quarter cycle decorrelates
/// the pair's beating without the anti-phase cancellation a half cycle would give
/// at zero detune.
const TWIN_PHASE: f32 = 0.25;

/// Plan a note-on under `mode`: state in, channel assignments out. Pure.
fn plan(mode: AssignMode, note: u8, unison_detune: f32, st: AllocView) -> Plan {
    let mut assigns = [Assign::default(); N];
    match mode {
        AssignMode::Poly => {
            // DCO behaviour: phase resets to zero (start_phase 0), no detune.
            assigns[0] = Assign {
                channel: allocate_one(note, st),
                detune_cents: 0.0,
                start_phase: 0.0,
            };
            Plan::new(assigns, 1)
        }
        AssignMode::Unison => {
            // Last-note priority: every channel retriggers to the new note (the
            // prior note is not stacked). Per-channel detune fans the copies out,
            // and a spread of start phases (rather than the Poly phase-0 reset)
            // decorrelates their beating so they don't comb into synchronised
            // nulls and thin the sound out.
            for (v, a) in assigns.iter_mut().enumerate() {
                *a = Assign {
                    channel: v,
                    detune_cents: unison_spread(v) * unison_detune,
                    start_phase: unison_phase(v),
                };
            }
            Plan::new(assigns, N)
        }
        AssignMode::Solo => {
            // One sounding channel; DCO phase reset, no detune. Reusing the active
            // channel gives legato glide (its glide source carries the last pitch).
            assigns[0] = Assign {
                channel: allocate_mono(st),
                detune_cents: 0.0,
                start_phase: 0.0,
            };
            Plan::new(assigns, 1)
        }
        AssignMode::Twin => {
            // Two channels per note: opposite ends of the detune fan, with the
            // second phase-decorrelated. `unison` falls out (len > 1) → the stack
            // gets the gentler glide scaling, and level_comp = 1/√2.
            let (a, b) = allocate_pair(note, st);
            assigns[0] = Assign {
                channel: a,
                detune_cents: -TWIN_SPREAD * unison_detune,
                start_phase: 0.0,
            };
            assigns[1] = Assign {
                channel: b,
                detune_cents: TWIN_SPREAD * unison_detune,
                start_phase: TWIN_PHASE,
            };
            Plan::new(assigns, 2)
        }
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;

    /// Bookkeeping arrays a view can borrow; mutate fields then call `.view()`.
    struct St {
        active: [bool; N],
        note: [u8; N],
        glide_semi: [f32; N],
        alloc_tick: [u64; N],
    }

    impl St {
        /// Empty layer: nothing active, every channel "free at pitch 0", tick 0.
        fn empty() -> Self {
            St {
                active: [false; N],
                note: [0; N],
                glide_semi: [0.0; N],
                alloc_tick: [0; N],
            }
        }

        fn view(&self) -> AllocView<'_> {
            AllocView {
                active: &self.active,
                note: &self.note,
                glide_semi: &self.glide_semi,
                alloc_tick: &self.alloc_tick,
            }
        }
    }

    #[test]
    fn poly_plan_is_one_undetuned_channel() {
        let st = St::empty();
        let p = plan(AssignMode::Poly, 60, 25.0, st.view());
        assert_eq!(p.len, 1);
        assert_eq!(p.assigns[0].detune_cents, 0.0);
        assert_eq!(p.assigns[0].start_phase, 0.0);
        assert_eq!(p.level_comp, 1.0);
        assert!(!p.unison);
    }

    #[test]
    fn poly_reuses_channel_already_on_note() {
        let mut st = St::empty();
        st.active[5] = true;
        st.note[5] = 60;
        assert_eq!(allocate_one(60, st.view()), 5);
    }

    #[test]
    fn poly_picks_nearest_free_by_glide() {
        let mut st = St::empty();
        // Channel 3's glide source sits closest to the new note (62).
        st.glide_semi = [10.0; N];
        st.glide_semi[3] = 61.0;
        assert_eq!(allocate_one(62, st.view()), 3);
    }

    #[test]
    fn poly_steals_oldest_when_full() {
        let mut st = St::empty();
        st.active = [true; N];
        // All on other notes (no reuse), none free → steal lowest alloc_tick.
        for v in 0..N {
            st.note[v] = 40 + v as u8;
            st.alloc_tick[v] = 100 + v as u64;
        }
        st.alloc_tick[6] = 1; // oldest
        assert_eq!(allocate_one(72, st.view()), 6);
    }

    #[test]
    fn unison_stacks_all_channels_symmetric() {
        let st = St::empty();
        let detune = 20.0;
        let p = plan(AssignMode::Unison, 60, detune, st.view());
        assert_eq!(p.len, N);
        assert!(p.unison);
        assert!((p.level_comp - 1.0 / (N as f32).sqrt()).abs() < 1e-6);
        // Every channel used exactly once, in order.
        for v in 0..N {
            assert_eq!(p.assigns[v].channel, v);
        }
        // Detune fans out symmetrically: ends at ∓detune, midpoint ~0.
        assert!((p.assigns[0].detune_cents + detune).abs() < 1e-6);
        assert!((p.assigns[N - 1].detune_cents - detune).abs() < 1e-6);
        let sum: f32 = p.iter().map(|a| a.detune_cents).sum();
        assert!(sum.abs() < 1e-4, "spread should sum ~0, got {sum}");
        // Start phases stay within the first half cycle.
        assert!(p.iter().all(|a| (0.0..=0.5).contains(&a.start_phase)));
    }

    #[test]
    fn solo_keeps_one_channel_when_nothing_active() {
        let st = St::empty();
        let p = plan(AssignMode::Solo, 60, 25.0, st.view());
        assert_eq!(p.len, 1);
        assert_eq!(p.assigns[0].channel, 0);
        assert_eq!(p.assigns[0].detune_cents, 0.0);
        assert_eq!(p.level_comp, 1.0);
        assert!(!p.unison);
    }

    #[test]
    fn solo_reuses_the_sounding_channel_for_legato() {
        let mut st = St::empty();
        // A note is already sounding on channel 4 (e.g. carried from a prior note
        // on a different pitch). Solo must take that same channel over, not a free
        // one, so glide is legato.
        st.active[4] = true;
        st.note[4] = 48;
        let p = plan(AssignMode::Solo, 72, 0.0, st.view());
        assert_eq!(p.assigns[0].channel, 4);
    }

    #[test]
    fn twin_uses_two_distinct_channels_spread_and_decorrelated() {
        let st = St::empty();
        let detune = 18.0;
        let p = plan(AssignMode::Twin, 60, detune, st.view());
        assert_eq!(p.len, 2);
        assert!(p.unison);
        assert!((p.level_comp - 1.0 / 2f32.sqrt()).abs() < 1e-6);
        let (a, b) = (p.assigns[0], p.assigns[1]);
        assert_ne!(a.channel, b.channel, "pair must be distinct channels");
        // Opposite ends of the detune fan; phases decorrelated.
        assert!((a.detune_cents + detune).abs() < 1e-6);
        assert!((b.detune_cents - detune).abs() < 1e-6);
        assert_ne!(a.start_phase, b.start_phase);
    }

    #[test]
    fn twin_reuses_both_channels_already_on_note() {
        let mut st = St::empty();
        // A Twin pair (channels 2 and 5) is already sounding this note.
        st.active[2] = true;
        st.note[2] = 64;
        st.active[5] = true;
        st.note[5] = 64;
        let (a, b) = allocate_pair(64, st.view());
        let mut pair = [a, b];
        pair.sort_unstable();
        assert_eq!(pair, [2, 5]);
    }

    #[test]
    fn twin_steals_two_distinct_oldest_when_full() {
        let mut st = St::empty();
        st.active = [true; N];
        for v in 0..N {
            st.note[v] = 40 + v as u8; // no reuse for the new note
            st.alloc_tick[v] = 100 + v as u64;
        }
        st.alloc_tick[3] = 1; // oldest
        st.alloc_tick[7] = 2; // next oldest
        let (a, b) = allocate_pair(80, st.view());
        let mut pair = [a, b];
        pair.sort_unstable();
        assert_eq!(pair, [3, 7]);
    }
}

#[cfg(test)]
mod mod_tests {
    use super::*;

    /// Neutral block context: every route Off, every depth / extra 0, key-track
    /// off. Non-route fields (waveforms, cutoff, …) carry harmless placeholders —
    /// `resolve_mod` never reads them. Tests mutate only what they assert via
    /// `ctx_with`, so the route under test isn't buried in fixture noise.
    fn neutral_ctx() -> BlockCtx {
        BlockCtx {
            os_sample_rate: 48_000.0,
            os: 1,
            osc1_wave: Waveform::Saw,
            osc2_wave: Waveform::Saw,
            osc1_level: 1.0,
            osc2_level: 1.0,
            ring_level: 0.0,
            osc1_pw: 0.5,
            osc2_pw: 0.5,
            osc1_semi: 0.0,
            osc2_semi: 0.0,
            cutoff: 1_000.0,
            hpf_cutoff: 20.0,
            resonance: 0.0,
            drive: 1.0,
            variant: LadderVariant::Sharp,
            base_semis: 0.0,
            lfo1_shape: LfoShape::Sine,
            lfo1_rate_hz: 1.0,
            lfo1_delay_time: 0.0,
            lfo1_fade: 0.0,
            lfo2_val: 0.0,
            sync: false,
            pm_index: 0.0,
            portamento_time: 0.0,
            pitch_lfo_sel: LfoSel::Off,
            pitch_lfo_depth: 0.0,
            pitch_env_sel: EnvSel::Off,
            pitch_env_depth: 0.0,
            pitch_extra: 0.0,
            pwm_lfo_sel: LfoSel::Off,
            pwm_lfo_depth: 0.0,
            pwm_env_sel: EnvSel::Off,
            pwm_env_depth: 0.0,
            pwm_extra: 0.0,
            cutoff_lfo1_depth: 0.0,
            cutoff_lfo2_depth: 0.0,
            cutoff_env_depth: 0.0,
            cutoff_vel_depth: 0.0,
            cutoff_extra: 0.0,
            filter_key_track: false,
            osc2_pitch_env_sel: EnvSel::Off,
            osc2_pitch_env_depth: 0.0,
            osc2_pitch_extra: 0.0,
        }
    }

    fn ctx_with(f: impl FnOnce(&mut BlockCtx)) -> BlockCtx {
        let mut c = neutral_ctx();
        f(&mut c);
        c
    }

    /// Plain sources: env levels, LFO values, velocity and note all explicit.
    fn src(e1: f32, e2: f32, lfo1: f32, lfo2: f32, velocity: f32, note: u8) -> ModSources {
        ModSources { e1, e2, lfo1, lfo2, velocity, note }
    }

    #[test]
    fn all_off_resolves_to_zero() {
        let ctx = neutral_ctx();
        let m = resolve_mod(&ctx, &src(1.0, 1.0, 1.0, 1.0, 1.0, 72));
        assert_eq!(m.pitch_mod, 0.0);
        assert_eq!(m.osc2_pitch_mod, 0.0);
        assert_eq!(m.pwm_mod, 0.0);
        assert_eq!(m.cutoff_mod, 0.0);
    }

    #[test]
    fn off_selector_ignores_its_source() {
        // Depth set, but selector Off → source must not leak through.
        let ctx = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Off;
            c.pitch_lfo_depth = 5.0;
        });
        let m = resolve_mod(&ctx, &src(0.0, 0.0, 1.0, 1.0, 0.0, 60));
        assert_eq!(m.pitch_mod, 0.0);
    }

    #[test]
    fn pitch_route_picks_selected_lfo_and_scales_by_depth() {
        let lfo1 = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 2.0;
        });
        // LFO1 = 0.5 → +1 st; LFO2 (= 0.9) must be ignored under the Lfo1 selector.
        let m = resolve_mod(&lfo1, &src(0.0, 0.0, 0.5, 0.9, 0.0, 60));
        assert!((m.pitch_mod - 1.0).abs() < 1e-6);

        let lfo2 = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo2;
            c.pitch_lfo_depth = 2.0;
        });
        let m = resolve_mod(&lfo2, &src(0.0, 0.0, 0.5, 0.9, 0.0, 60));
        assert!((m.pitch_mod - 1.8).abs() < 1e-6);
    }

    #[test]
    fn pitch_route_sums_lfo_env_and_wheel_extra() {
        let ctx = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 2.0; // 0.5 → +1.0
            c.pitch_env_sel = EnvSel::Env1;
            c.pitch_env_depth = 3.0; // 0.5 → +1.5
            c.pitch_extra = 0.25; // pitch wheel
        });
        let m = resolve_mod(&ctx, &src(0.5, 0.0, 0.5, 0.0, 0.0, 60));
        assert!((m.pitch_mod - 2.75).abs() < 1e-6);
    }

    #[test]
    fn osc2_pitch_route_is_env_plus_extra_independent_of_common_pitch() {
        let ctx = ctx_with(|c| {
            c.osc2_pitch_env_sel = EnvSel::Env2;
            c.osc2_pitch_env_depth = 12.0; // 0.5 → +6 st
            c.osc2_pitch_extra = 1.0;
            c.pitch_lfo_sel = LfoSel::Lfo1; // common-pitch route must not bleed in
            c.pitch_lfo_depth = 10.0;
        });
        let m = resolve_mod(&ctx, &src(0.0, 0.5, 1.0, 0.0, 0.0, 60));
        assert!((m.osc2_pitch_mod - 7.0).abs() < 1e-6);
    }

    #[test]
    fn cutoff_velocity_route() {
        let ctx = ctx_with(|c| c.cutoff_vel_depth = 24.0);
        let m = resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.5, 60));
        assert!((m.cutoff_mod - 12.0).abs() < 1e-6);
    }

    #[test]
    fn cutoff_keytrack_pivots_at_c4_one_octave_per_octave() {
        let ctx = ctx_with(|c| c.filter_key_track = true);
        assert_eq!(resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 60)).cutoff_mod, 0.0);
        assert_eq!(resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 72)).cutoff_mod, 12.0);
        assert_eq!(resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 48)).cutoff_mod, -12.0);
    }

    #[test]
    fn keytrack_off_ignores_note() {
        let ctx = neutral_ctx(); // key-track off
        assert_eq!(resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 96)).cutoff_mod, 0.0);
    }

    #[test]
    fn negative_depth_inverts_route() {
        let ctx = ctx_with(|c| {
            c.pwm_lfo_sel = LfoSel::Lfo2;
            c.pwm_lfo_depth = -0.4;
        });
        let m = resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 1.0, 0.0, 60));
        assert!((m.pwm_mod + 0.4).abs() < 1e-6);
    }

    #[test]
    fn routes_are_independent_no_cross_talk() {
        // Every route wired to a distinct source; each destination must reflect
        // only its own selection.
        let ctx = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 1.0;
            c.pwm_env_sel = EnvSel::Env1;
            c.pwm_env_depth = 1.0;
            c.cutoff_lfo2_depth = 1.0; // fixed LFO 2 → cutoff
        });
        let m = resolve_mod(&ctx, &src(0.3, 0.0, 0.7, 0.2, 0.0, 60));
        assert!((m.pitch_mod - 0.7).abs() < 1e-6);
        assert!((m.pwm_mod - 0.3).abs() < 1e-6);
        assert!((m.cutoff_mod - 0.2).abs() < 1e-6);
        assert_eq!(m.osc2_pitch_mod, 0.0);
    }

    // ── block_glide ──

    #[test]
    fn glide_zero_time_snaps() {
        // Time 0 is glide off (no separate on/off), and never a divide-by-zero.
        assert_eq!(block_glide(0.0, false, 64, 48_000.0), (false, 1.0));
    }

    #[test]
    fn glide_coeff_is_block_size_independent() {
        // Same wall-clock duration via different (frames, rate) pairs → same coeff.
        let (_, a) = block_glide(0.2, false, 64, 48_000.0);
        let (_, b) = block_glide(0.2, false, 32, 24_000.0);
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        assert!(a > 0.0 && a < 1.0);
    }

    #[test]
    fn unison_glides_slower_than_poly() {
        // Shorter effective time → larger coefficient (faster approach per block).
        let (_, poly) = block_glide(0.2, false, 64, 48_000.0);
        let (_, uni) = block_glide(0.2, true, 64, 48_000.0);
        assert!(uni > poly, "unison {uni} should exceed poly {poly}");
    }

    // ── envelopes_static ──

    /// Per-voice env arrays with channel `v` parked in `stage`.
    fn envs_all(stage: AdsrStage) -> ([AdsrCore; N], [AdsrCore; N]) {
        let mut a = std::array::from_fn(|_| AdsrCore::new(48_000.0));
        let mut b = std::array::from_fn(|_| AdsrCore::new(48_000.0));
        for v in 0..N {
            a[v].stage = stage;
            b[v].stage = stage;
        }
        (a, b)
    }

    #[test]
    fn static_when_all_sustain_gate_high_no_trigger() {
        let (e1, e2) = envs_all(AdsrStage::Sustain);
        assert!(envelopes_static(&[false; N], &[true; N], &[true; N], &e1, &e2));
    }

    #[test]
    fn not_static_when_any_voice_triggers() {
        let (e1, e2) = envs_all(AdsrStage::Sustain);
        let mut trig = [false; N];
        trig[3] = true;
        assert!(!envelopes_static(&trig, &[true; N], &[true; N], &e1, &e2));
    }

    #[test]
    fn not_static_when_a_voice_is_mid_attack() {
        let (mut e1, e2) = envs_all(AdsrStage::Sustain);
        e1[5].stage = AdsrStage::Attack; // env1 not settled
        assert!(!envelopes_static(&[false; N], &[true; N], &[true; N], &e1, &e2));
    }

    #[test]
    fn not_static_when_gate_released_but_active() {
        let (e1, e2) = envs_all(AdsrStage::Sustain);
        let mut gate = [true; N];
        gate[2] = false; // releasing voice must take the per-frame path (can free)
        assert!(!envelopes_static(&[false; N], &[true; N], &gate, &e1, &e2));
    }

    #[test]
    fn inactive_voices_ignored() {
        // An inactive voice in any stage / gate doesn't block the static path.
        let (mut e1, mut e2) = envs_all(AdsrStage::Sustain);
        e1[7].stage = AdsrStage::Release;
        e2[7].stage = AdsrStage::Idle;
        let mut active = [true; N];
        active[7] = false;
        let mut gate = [true; N];
        gate[7] = false;
        assert!(envelopes_static(&[false; N], &active, &gate, &e1, &e2));
    }
}
