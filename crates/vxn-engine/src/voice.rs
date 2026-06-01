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
    AdsrCore, AdsrShape, AdsrStage, CHANNELS_PER_LAYER, CONTROL_BLOCK, FilterMode, FilterSlope,
    LfoCore, LfoShape, NoiseColor, OtaLadderCoeffs, PolyHpf, PolyNoiseBank, PolyOscillator,
    PolyOtaLadder, Waveform, fast_exp2, note_to_hz, poly_ring_mod, poly_sub_square, xorshift64,
};

use crate::params::{AssignMode, CrossModType, EnvSel, LfoSel};

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
    /// Sub-oscillator mix level (square one octave below osc1, phase-locked).
    /// 0 = no-op path (no sub kernel, no flipflop read).
    pub sub_level: f32,
    /// `CrossModType::Ring` engaged: osc1×osc2 ring displaces osc1 in the
    /// mixer slot at full amplitude (osc1_level then sets the ring's mix).
    /// Mutually exclusive with `sync` / `pm_index`; off = the fast path.
    pub ring_mode: bool,
    /// Noise source mix level. 0 = the cheap no-op path (no PRNG/pink work).
    pub noise_level: f32,
    /// Noise colour (White / Pink); layer-wide, so the branch hoists.
    pub noise_color: NoiseColor,
    pub osc1_pw: f32,
    pub osc2_pw: f32,
    pub osc1_semi: f32,
    pub osc2_semi: f32,
    pub cutoff: f32,
    /// Pre-VCF high-pass cutoff (Hz). 20 ≈ open / "off".
    pub hpf_cutoff: f32,
    pub resonance: f32,
    pub drive: f32,
    /// OTA filter response (LP / HP / BP / Notch) and slope (2- vs 4-pole).
    pub filter_mode: FilterMode,
    pub filter_slope: FilterSlope,
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
    /// Cross-mod selector value (Off / Sync / Pm / Ring). Drives the routing
    /// decisions for env→pitch (when `pitch_env_mod_only`) and the mod-wheel
    /// sweep — both target the "modulator" oscillator under Sync (osc1) and
    /// Pm (osc2). The kernel dispatch keeps using `sync` / `pm_index` so an
    /// FM patch with amount = 0 still takes the cheap fast path, but the
    /// routing here must read the *mode*, not the amount.
    pub cross_mod_type: CrossModType,
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
    /// When true the LFO→pitch contribution is diverted to the cross-mod
    /// "modulator" oscillator (same routing as [`Self::pitch_env_mod_only`]).
    /// Lets the player vibrato the modulator (e.g. FM index wobble) without
    /// moving the carrier pitch.
    pub pitch_lfo_mod_only: bool,
    pub pitch_env_sel: EnvSel,
    pub pitch_env_depth: f32,
    /// When true the env→pitch contribution is routed to the cross-mod
    /// "modulator" oscillator only: Sync → osc1 (slave whose pitch creates the
    /// sync sweep), PM → osc2 (modulator whose pitch sets the FM index); Off /
    /// Ring have no modulator role, so the env falls back to both oscillators
    /// (no-op vs. the toggle off). When false the env always moves both
    /// oscillators like pitch-wheel.
    pub pitch_env_mod_only: bool,
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
    /// Mod-wheel → sweep contribution (semitones). Target depends on cross-mod
    /// mode: Off → both oscs, Sync → osc1 (slave/carrier whose pitch creates the
    /// sync sweep), PM → osc2 (modulator whose pitch sets the FM index/spectrum).
    pub sweep_extra: f32,
    // ── VCA modulation ──
    /// LFO source for amp tremolo ({Off/LFO1/LFO2}); always applied on top of the
    /// amp envelope / gate.
    pub amp_lfo_sel: LfoSel,
    /// Tremolo depth (0..1): the LFO attenuates the VCA between `1-depth` and 1.
    pub amp_lfo_depth: f32,
    /// Env-bypass: when true the VCA follows the bare note gate at full level
    /// instead of Env 2's ADSR shape (gate / organ mode). Tremolo still applies.
    pub amp_env_bypass: bool,
}

/// All 16 voices in structure-of-arrays form.
pub struct VoiceBank {
    osc1: PolyOscillator,
    osc2: PolyOscillator,
    noise: PolyNoiseBank,
    hpf: PolyHpf,
    ladder: PolyOtaLadder,
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
    /// Free-running PRNG state for Unison start-phase randomisation. Each Unison
    /// note-on draws one fresh phase per channel from this, decorrelating the
    /// stack's beating without a deterministic comb (0011). Seeded non-zero from
    /// the layer's base seed and never reset, so repeated note-ons stay varied.
    unison_rng: u64,
    /// Mono (Solo / Unison) held-note stack, newest on top. The voice sounds only
    /// the top entry; on note-off the top is popped and the bank reverts to whatever
    /// is still held (ADR 0003 §4 / 0010). Fixed-size — allocation free; an
    /// overflowing key drops the oldest held note.
    mono_stack: [u8; MONO_STACK],
    mono_len: usize,
}

/// Capacity of the mono held-note stack. Far beyond ten fingers; an overflow
/// drops the oldest held note rather than allocating.
const MONO_STACK: usize = 32;

/// Decorrelated per-channel LFO 1 seed from the layer's base seed.
#[inline]
fn lfo1_seed(base: u64, ch: usize) -> u64 {
    base.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((ch as u64 + 1).wrapping_mul(0x632B_E5A6))
}

/// Unison phase-RNG seed from the layer's base seed, on a stream distinct from
/// the LFO seeds and forced non-zero (xorshift64 sticks at zero).
#[inline]
fn unison_rng_seed(base: u64) -> u64 {
    base.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1
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
            noise: PolyNoiseBank::new(rng_seed),
            hpf: PolyHpf::new(),
            ladder: PolyOtaLadder::new(),
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
            unison_rng: unison_rng_seed(rng_seed),
            mono_stack: [0; MONO_STACK],
            mono_len: 0,
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
        self.mono_len = 0;
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

    /// Channel `v`'s gated note (`None` when the channel isn't sounding). Lets
    /// tests assert which note a layer is voicing and on which channel.
    #[cfg(test)]
    pub(crate) fn gated_note(&self, v: usize) -> Option<u8> {
        (self.active[v] && self.gate[v]).then_some(self.note[v])
    }

    /// Whether channel `v` has a pending retrigger this block. A legato re-pitch
    /// leaves it false; a fresh trigger sets it — so tests can tell them apart.
    #[cfg(test)]
    pub(crate) fn trigger_pending(&self, v: usize) -> bool {
        self.trigger_pending[v]
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
    #[allow(clippy::too_many_arguments)] // one coupled per-note param set, single caller
    pub fn note_on(
        &mut self,
        mode: AssignMode,
        note: u8,
        velocity: f32,
        alloc_tick: u64,
        unison_detune: f32,
        lfo1: Lfo1Trigger,
        legato: bool,
    ) {
        // Solo and Unison are monophonic in spirit — one logical note (Solo on one
        // channel, Unison stacked across all, detuned) — so both run the stateful
        // mono path: a held-note stack, legato re-pitch, and quiescing of unused
        // channels. Poly/Twin take the pure `plan` policy below.
        if matches!(mode, AssignMode::Solo | AssignMode::Unison) {
            let was_sounding = self.mono_len > 0;
            self.mono_push(note);
            let slide = legato && was_sounding;
            self.mono_voice(mode, note, velocity, alloc_tick, unison_detune, lfo1, slide);
            return;
        }
        // Leaving a mono mode discards the held-note stack so a later return starts
        // clean rather than reviving stale notes.
        self.mono_len = 0;
        // Decide *which* channels and their detune/phase purely from bookkeeping
        // (`plan`), then apply the DSP effect (`trigger`) per assignment. The
        // borrow in `alloc_view` ends when `plan` returns its owned result, so the
        // mutating `trigger` calls below are free to touch the same arrays.
        let mut rng = self.unison_rng;
        let plan = plan(mode, note, unison_detune, self.alloc_view(), &mut rng);
        self.unison_rng = rng;
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

    /// Reference channel for a mono voice — always sounding (Solo's single channel,
    /// and channel 0 of the Unison stack), so it carries the voice's live velocity.
    const MONO_REF_CH: usize = 0;

    /// Voice a mono note (Solo / Unison): apply the mode's channel plan, then quiesce
    /// every channel the plan doesn't use so the voice is strictly monophonic. With
    /// `slide` (legato over a still-held note) the planned channels only change pitch
    /// — no envelope/phase retrigger, so the glide carries them; otherwise they
    /// retrigger. Solo plans one channel (pinned to 0); Unison plans all, detuned.
    #[allow(clippy::too_many_arguments)] // one coupled per-note param set
    fn mono_voice(
        &mut self,
        mode: AssignMode,
        note: u8,
        velocity: f32,
        alloc_tick: u64,
        unison_detune: f32,
        lfo1: Lfo1Trigger,
        slide: bool,
    ) {
        let mut rng = self.unison_rng;
        let plan = plan(mode, note, unison_detune, self.alloc_view(), &mut rng);
        self.unison_rng = rng;
        let mut used = [false; N];
        for a in plan.iter() {
            used[a.channel] = true;
            if slide {
                self.repitch(a.channel, note, velocity, a.detune_cents);
            } else {
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
        }
        // Quiesce channels the voice doesn't use (Solo's other 7; tails left from a
        // prior Poly chord or a mode switch).
        for (v, &u) in used.iter().enumerate() {
            if !u {
                self.gate[v] = false;
            }
        }
        self.level_comp = plan.level_comp;
        self.unison = plan.unison;
    }

    /// Push a note onto the mono held-note stack, newest on top. A repeated note is
    /// moved to the top rather than duplicated; an overflow drops the oldest.
    fn mono_push(&mut self, note: u8) {
        if let Some(i) = self.mono_stack[..self.mono_len]
            .iter()
            .position(|&n| n == note)
        {
            self.mono_stack.copy_within(i + 1..self.mono_len, i);
            self.mono_len -= 1;
        } else if self.mono_len == MONO_STACK {
            self.mono_stack.copy_within(1..MONO_STACK, 0);
            self.mono_len -= 1;
        }
        self.mono_stack[self.mono_len] = note;
        self.mono_len += 1;
    }

    /// Change channel `v`'s pitch without retriggering: keeps the envelope, LFO and
    /// oscillator phases running and lets the block-rate glide slide to the new note
    /// (legato). The channel must already be sounding. Detune is restamped (the same
    /// fixed Unison spread) so the stack stays correctly fanned across a slur.
    fn repitch(&mut self, v: usize, note: u8, velocity: f32, detune_cents: f32) {
        self.note[v] = note;
        self.velocity[v] = velocity;
        self.detune_cents[v] = detune_cents;
        self.gate[v] = true;
        self.active[v] = true;
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

    /// Mono note-off (Solo / Unison, ADR 0003 §4): remove the key from the held-note
    /// stack and release every channel sounding it. If it was the sounding (top)
    /// note and others are still held, revert the voice to the newest of those —
    /// `legato` reverts without retriggering (slurred), else the revealed note
    /// retriggers. Releasing a held key that wasn't sounding just drops it from the
    /// stack.
    #[allow(clippy::too_many_arguments)] // mirrors the mono note-on param set
    pub fn mono_note_off(
        &mut self,
        mode: AssignMode,
        note: u8,
        legato: bool,
        alloc_tick: u64,
        unison_detune: f32,
        lfo1: Lfo1Trigger,
    ) {
        let was_top = self.mono_pop(note);
        // Release every channel sounding this note: the mono voice itself, plus any
        // stray channel left holding it from before a mode switch.
        for v in 0..N {
            if self.gate[v] && self.note[v] == note {
                self.gate[v] = false;
            }
        }
        if !was_top || self.mono_len == 0 {
            return;
        }
        // Revert the voice to the newest still-held note (legato carries the slide).
        let revealed = self.mono_stack[self.mono_len - 1];
        let vel = self.velocity[Self::MONO_REF_CH];
        self.mono_voice(mode, revealed, vel, alloc_tick, unison_detune, lfo1, legato);
    }

    /// Remove `note` from the mono stack; returns whether it was the top (sounding)
    /// entry, so the caller knows whether the sounding note must change.
    fn mono_pop(&mut self, note: u8) -> bool {
        match self.mono_stack[..self.mono_len]
            .iter()
            .position(|&n| n == note)
        {
            Some(i) => {
                let was_top = i + 1 == self.mono_len;
                self.mono_stack.copy_within(i + 1..self.mono_len, i);
                self.mono_len -= 1;
                was_top
            }
            None => false,
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
        // Amp tremolo gain per voice (block-rate): the selected LFO attenuates the
        // VCA between `1 - depth` and 1. 1.0 = no tremolo (the common path).
        let mut amp_trem = [1.0f32; N];
        for v in 0..N {
            // LFO 1's onset gain ramps per frame, so it's applied here (reading the
            // per-voice onset state) before handing a plain value to `resolve_mod`.
            let lfo1 = lfo1_raw[v] * self.lfo1_onset.gain(v, ctx.lfo1_delay_time, ctx.lfo1_fade);
            // Amp tremolo: attenuate-only, so the VCA can't exceed unity (lfo=+1 →
            // gain 1, lfo=-1 → gain 1-depth). Reads the per-voice onset-scaled LFO1.
            amp_trem[v] = 1.0
                - ctx.amp_lfo_depth * 0.5 * (1.0 - lfo_src(ctx.amp_lfo_sel, lfo1, ctx.lfo2_val));
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
            // Env/LFO→pitch with the "Mod" switch on: isolate to the
            // modulator oscillator. Default modulator = osc2; Sync flips to
            // osc1 (the slave that drives the sweep). Off / Ring have no
            // cross-mod role but the toggle is still a user-visible
            // "isolate to one osc" affordance, so it routes to osc2 — same
            // as Pm — instead of falling back to both oscs (which would
            // make the switch a no-op when no cross-mod is in play).
            let (mod_only_to_osc1, mod_only_to_osc2) = match ctx.cross_mod_type {
                CrossModType::Sync => (m.pitch_mod_only, 0.0),
                _ => (0.0, m.pitch_mod_only),
            };
            // Mod-wheel cross-mod sweep follows the *cross-mod role*
            // strictly: Off / Ring sweep both oscs (whole-note pitch
            // effect, no modulator); Sync sweeps the slave (osc1); Pm
            // sweeps the modulator (osc2) for FM index/spectrum.
            let (sweep_to_osc1, sweep_to_osc2) = match ctx.cross_mod_type {
                CrossModType::Off | CrossModType::Ring => (m.sweep_mod, m.sweep_mod),
                CrossModType::Sync => (m.sweep_mod, 0.0),
                CrossModType::Pm => (0.0, m.sweep_mod),
            };
            let s1 = ctx.base_semis
                + nf
                + ctx.osc1_semi
                + m.pitch_mod
                + mod_only_to_osc1
                + sweep_to_osc1
                + detune;
            let s2 = ctx.base_semis
                + nf
                + ctx.osc2_semi
                + m.pitch_mod
                + mod_only_to_osc2
                + sweep_to_osc2
                + detune;
            self.osc1.inc[v] = note_to_hz(s1) / ctx.os_sample_rate;
            self.osc2.inc[v] = note_to_hz(s2) / ctx.os_sample_rate;
            pw1[v] = (ctx.osc1_pw + m.pwm_mod).clamp(0.05, 0.95);
            pw2[v] = (ctx.osc2_pw + m.pwm_mod).clamp(0.05, 0.95);

            let cutoff_hz = ctx.cutoff * fast_exp2(m.cutoff_mod / 12.0);
            self.ladder.set_coeffs(
                v,
                OtaLadderCoeffs::new(cutoff_hz, ctx.os_sample_rate, ctx.resonance, ctx.drive),
            );
        }
        // Filter response is layer-wide, not per voice.
        self.ladder.set_response(ctx.filter_mode, ctx.filter_slope);

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
        let mut sub = [0.0f32; N];
        let mut noise = [0.0f32; N];
        let mut mix = [0.0f32; N];
        let mut hp = [0.0f32; N];
        let mut filt = [0.0f32; N];
        let mut amp = [0.0f32; N];

        // Ring modulator (0021, 0061): osc1×osc2 through the Parker diode bridge,
        // routed into the osc1 mixer slot when `CrossModType::Ring` is engaged.
        // Off (any other cross-mod mode) skips the diode maths entirely.
        let ring_on = ctx.ring_mode;
        let ring_gain = 10.0f32.powf(RING_DRIVE_DB / 20.0);
        // Noise source mixed into the source bus; zero level skips PRNG/pink work.
        let noise_on = ctx.noise_level != 0.0;
        // Sub-osc (0062): square one octave below the source osc, phase-locked
        // to its flipflop. Source is osc2 under Sync (audible period = master),
        // osc1 otherwise (Off/Ring/FM). Zero level skips the kernel.
        let sub_on = ctx.sub_level != 0.0;

        // Envelope block-skip (see `envelopes_static`): when nothing triggers and
        // every active voice holds both envelopes in Sustain, the env levels are
        // constant, so `amp` is computed once and the per-frame tick + free-check
        // are skipped. Otherwise the per-frame path runs.
        let env_static = envelopes_static(&trig, &self.active, &self.gate, &self.env1, &self.env2);
        if env_static {
            for (v, amp_v) in amp.iter_mut().enumerate() {
                *amp_v = amp_base(
                    self.active[v],
                    self.gate[v],
                    ctx.amp_env_bypass,
                    self.env2[v].level,
                ) * amp_trem[v];
            }
        }

        for base_i in 0..base_frames {
            // Envelopes + amp (base rate, scalar; gated to 0 for inactive voices).
            // The VCA follows Env2 unless `amp_env_bypass` (then the bare gate),
            // times the block-rate tremolo gain. Env2 still ticks in bypass so the
            // voice frees on release as usual; Env1 ticks to feed the mod routes.
            // Skipped when the block is envelope-static (see `env_static` above).
            if !env_static {
                for v in 0..N {
                    let t = trig[v] && base_i == 0;
                    let _e1 = self.env1[v].tick(t, self.gate[v]);
                    let e2 = self.env2[v].tick(t, self.gate[v]);
                    amp[v] = amp_base(self.active[v], self.gate[v], ctx.amp_env_bypass, e2)
                        * amp_trem[v];
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
                // Ring displaces osc1 in the mixer slot when engaged — osc1_level
                // then controls the ring's loudness; osc2 stays independently mixable.
                if ring_on {
                    poly_ring_mod(&o1, &o2, ring_gain, &mut ring);
                    for v in 0..N {
                        mix[v] = ring[v] * ctx.osc1_level + o2[v] * ctx.osc2_level;
                    }
                } else {
                    for v in 0..N {
                        mix[v] = o1[v] * ctx.osc1_level + o2[v] * ctx.osc2_level;
                    }
                }
                // Sub: keyed to osc2 under Sync (audible period = master), osc1
                // otherwise. The flipflop the source kernel toggled lives on osc1
                // (the audible carrier) in all modes.
                if sub_on {
                    let (sp, sdt) = if ctx.sync {
                        (&self.osc2.phase, &self.osc2.inc)
                    } else {
                        (&self.osc1.phase, &self.osc1.inc)
                    };
                    poly_sub_square(sp, sdt, &self.osc1.sub_flipflop, &mut sub);
                    for v in 0..N {
                        mix[v] += sub[v] * ctx.sub_level;
                    }
                }
                // Noise contribution: one decorrelated stream per voice, summed in.
                if noise_on {
                    self.noise.process(ctx.noise_color, &mut noise);
                    for v in 0..N {
                        mix[v] += noise[v] * ctx.noise_level;
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

/// VCA amp base level for one voice (before tremolo): 0 when inactive, the bare
/// note gate at full level when `bypass` is on (gate / organ mode), else the
/// Env 2 level clamped non-negative. The caller multiplies in the tremolo gain.
#[inline]
fn amp_base(active: bool, gate: bool, bypass: bool, env2_level: f32) -> f32 {
    if !active {
        0.0
    } else if bypass {
        if gate { 1.0 } else { 0.0 }
    } else {
        env2_level.max(0.0)
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

/// Resolved per-channel mod offsets: pitch / cross-mod-sweep / cutoff in semitones,
/// pwm as a pulse-width fraction. `pitch_mod` is the common pitch channel
/// (LFO + env when neither is mod-only + pitch wheel) added to both oscs;
/// `pitch_mod_only` is the sum of LFO and Env contributions whose "Mod"
/// switch is on, routed to the cross-mod modulator in `render_block` (Sync →
/// osc1, FM → osc2, Off/Ring → both). `sweep_mod` is the mod-wheel cross-mod
/// sweep channel; its osc target depends on cross-mod mode (gated in
/// `render_block`).
struct ModOut {
    pitch_mod: f32,
    pitch_mod_only: f32,
    sweep_mod: f32,
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
    // LFO→pitch and Env→pitch are each split: diverted to the cross-mod
    // "modulator" channel when their "Mod" switch is on, else joining the
    // common-pitch channel (both oscs). Pitch-wheel always moves both oscs.
    let pitch_lfo = lfo_src(ctx.pitch_lfo_sel, s.lfo1, s.lfo2) * ctx.pitch_lfo_depth;
    let (pitch_lfo_common, pitch_lfo_mod) = if ctx.pitch_lfo_mod_only {
        (0.0, pitch_lfo)
    } else {
        (pitch_lfo, 0.0)
    };
    let pitch_env = env_src(ctx.pitch_env_sel, s.e1, s.e2) * ctx.pitch_env_depth;
    let (pitch_env_common, pitch_env_mod) = if ctx.pitch_env_mod_only {
        (0.0, pitch_env)
    } else {
        (pitch_env, 0.0)
    };
    ModOut {
        pitch_mod: pitch_lfo_common + pitch_env_common + ctx.pitch_extra,
        pitch_mod_only: pitch_lfo_mod + pitch_env_mod,
        // Mod-wheel cross-mod sweep (sync sweeps / FM index / both-osc pitch).
        // The target osc(s) are chosen in `render_block` per cross-mod mode.
        sweep_mod: ctx.sweep_extra,
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

/// A fresh random Unison start phase in `[0, 1)` drawn from `rng` (xorshift64
/// mapped from `[-1, 1]`). One draw per voice per trigger decorrelates the
/// stack's beating. Random (not a fixed even spread) is deliberate: a full even
/// spread sums to zero for coherent copies (detune 0), gutting the level, whereas
/// independent random phases sum as a random walk (~`√N`) that `level_comp`'s
/// `1/√N` normalises — no systematic comb null at any detune.
#[inline]
fn random_phase(rng: &mut u64) -> f32 {
    // `xorshift64` spans `[-1, 1]` inclusive; `.fract()` folds the lone 1.0
    // endpoint back to 0.0 so the stamped phase never lands on the wrap point.
    ((xorshift64(rng) + 1.0) * 0.5).fract()
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

/// Twin pitch spread reuses the Unison extremes (±`UnisonDetune` cents); the two
/// channels sit at the opposite ends of that fan.
const TWIN_SPREAD: f32 = 1.0;
/// Twin start-phase offset for the second channel — a quarter cycle decorrelates
/// the pair's beating without the anti-phase cancellation a half cycle would give
/// at zero detune.
const TWIN_PHASE: f32 = 0.25;

/// Plan a note-on under `mode`: state in, channel assignments out. Pure except
/// for the Unison arm, which draws one random start phase per voice from `rng`
/// (the only arm that touches it — other modes leave the stream untouched, so
/// they stay fully deterministic).
fn plan(mode: AssignMode, note: u8, unison_detune: f32, st: AllocView, rng: &mut u64) -> Plan {
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
            // and a random start phase per voice on each trigger (rather than the
            // Poly phase-0 reset, or a fixed even spread) decorrelates their
            // beating so they don't comb into synchronised nulls and thin out.
            for (v, a) in assigns.iter_mut().enumerate() {
                *a = Assign {
                    channel: v,
                    detune_cents: unison_spread(v) * unison_detune,
                    start_phase: random_phase(rng),
                };
            }
            Plan::new(assigns, N)
        }
        AssignMode::Solo => {
            // Monophonic, pinned to channel 0 (every other channel stays
            // quiescent). The stateful stack / legato / retrigger decisions live in
            // `VoiceBank::solo_note_on`; this pure arm only fixes the channel so the
            // policy stays total and testable. `note`/`st` are unused here.
            let _ = (note, st);
            assigns[0] = Assign {
                channel: 0,
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
        let p = plan(AssignMode::Poly, 60, 25.0, st.view(), &mut 1);
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
        let mut rng = 0x1234_5678u64;
        let p = plan(AssignMode::Unison, 60, detune, st.view(), &mut rng);
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
        // Start phases are random in [0, 1) and decorrelated (not all equal).
        assert!(p.iter().all(|a| (0.0..1.0).contains(&a.start_phase)));
        let phases: Vec<f32> = p.iter().map(|a| a.start_phase).collect();
        assert!(
            phases.windows(2).any(|w| w[0] != w[1]),
            "random phases should vary, got {phases:?}"
        );
    }

    #[test]
    fn solo_keeps_one_channel_when_nothing_active() {
        let st = St::empty();
        let p = plan(AssignMode::Solo, 60, 25.0, st.view(), &mut 1);
        assert_eq!(p.len, 1);
        assert_eq!(p.assigns[0].channel, 0);
        assert_eq!(p.assigns[0].detune_cents, 0.0);
        assert_eq!(p.level_comp, 1.0);
        assert!(!p.unison);
    }

    #[test]
    fn solo_pins_to_channel_zero_regardless_of_active_channels() {
        let mut st = St::empty();
        // Even with another channel sounding (e.g. a tail from a prior chord), Solo
        // is pinned to channel 0 so the mono voice is deterministic and every other
        // channel is left to be quiesced by the bank.
        st.active[4] = true;
        st.note[4] = 48;
        let p = plan(AssignMode::Solo, 72, 0.0, st.view(), &mut 1);
        assert_eq!(p.assigns[0].channel, 0);
    }

    #[test]
    fn twin_uses_two_distinct_channels_spread_and_decorrelated() {
        let st = St::empty();
        let detune = 18.0;
        let p = plan(AssignMode::Twin, 60, detune, st.view(), &mut 1);
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
            sub_level: 0.0,
            ring_mode: false,
            noise_level: 0.0,
            noise_color: NoiseColor::White,
            osc1_pw: 0.5,
            osc2_pw: 0.5,
            osc1_semi: 0.0,
            osc2_semi: 0.0,
            cutoff: 1_000.0,
            hpf_cutoff: 20.0,
            resonance: 0.0,
            drive: 1.0,
            filter_mode: FilterMode::Lp,
            filter_slope: FilterSlope::Pole4,
            base_semis: 0.0,
            lfo1_shape: LfoShape::Sine,
            lfo1_rate_hz: 1.0,
            lfo1_delay_time: 0.0,
            lfo1_fade: 0.0,
            lfo2_val: 0.0,
            sync: false,
            pm_index: 0.0,
            cross_mod_type: CrossModType::Off,
            portamento_time: 0.0,
            pitch_lfo_sel: LfoSel::Off,
            pitch_lfo_depth: 0.0,
            pitch_lfo_mod_only: false,
            pitch_env_sel: EnvSel::Off,
            pitch_env_depth: 0.0,
            pitch_env_mod_only: false,
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
            sweep_extra: 0.0,
            amp_lfo_sel: LfoSel::Off,
            amp_lfo_depth: 0.0,
            amp_env_bypass: false,
        }
    }

    fn ctx_with(f: impl FnOnce(&mut BlockCtx)) -> BlockCtx {
        let mut c = neutral_ctx();
        f(&mut c);
        c
    }

    /// Plain sources: env levels, LFO values, velocity and note all explicit.
    fn src(e1: f32, e2: f32, lfo1: f32, lfo2: f32, velocity: f32, note: u8) -> ModSources {
        ModSources {
            e1,
            e2,
            lfo1,
            lfo2,
            velocity,
            note,
        }
    }

    #[test]
    fn all_off_resolves_to_zero() {
        let ctx = neutral_ctx();
        let m = resolve_mod(&ctx, &src(1.0, 1.0, 1.0, 1.0, 1.0, 72));
        assert_eq!(m.pitch_mod, 0.0);
        assert_eq!(m.sweep_mod, 0.0);
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
    fn sweep_route_is_mod_wheel_only_independent_of_common_pitch() {
        let ctx = ctx_with(|c| {
            c.sweep_extra = 1.0;
            c.pitch_lfo_sel = LfoSel::Lfo1; // common-pitch route must not bleed in
            c.pitch_lfo_depth = 10.0;
        });
        let m = resolve_mod(&ctx, &src(0.0, 0.5, 1.0, 0.0, 0.0, 60));
        assert!((m.sweep_mod - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_env_mod_only_diverts_env_to_modulator_channel() {
        // Toggle off: env joins the common pitch channel (both oscs).
        let common = ctx_with(|c| {
            c.pitch_env_sel = EnvSel::Env1;
            c.pitch_env_depth = 4.0; // 0.5 → +2 st
            c.pitch_env_mod_only = false;
        });
        let m = resolve_mod(&common, &src(0.5, 0.0, 0.0, 0.0, 0.0, 60));
        assert!((m.pitch_mod - 2.0).abs() < 1e-6);
        assert_eq!(m.pitch_mod_only, 0.0);
        // Toggle on: env leaves common-pitch and rides the mod-only channel;
        // the per-mode osc routing is applied downstream in render_block.
        let mod_only = ctx_with(|c| {
            c.pitch_env_sel = EnvSel::Env1;
            c.pitch_env_depth = 4.0;
            c.pitch_env_mod_only = true;
        });
        let m = resolve_mod(&mod_only, &src(0.5, 0.0, 0.0, 0.0, 0.0, 60));
        assert_eq!(m.pitch_mod, 0.0);
        assert!((m.pitch_mod_only - 2.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_lfo_mod_only_diverts_lfo_to_modulator_channel() {
        // Toggle off: LFO joins the common pitch channel (both oscs).
        let common = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 2.0; // 0.5 → +1 st
            c.pitch_lfo_mod_only = false;
        });
        let m = resolve_mod(&common, &src(0.0, 0.0, 0.5, 0.0, 0.0, 60));
        assert!((m.pitch_mod - 1.0).abs() < 1e-6);
        assert_eq!(m.pitch_mod_only, 0.0);
        // Toggle on: LFO rides the mod-only channel; the cross-mod target is
        // applied downstream in `render_block`.
        let mod_only = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 2.0;
            c.pitch_lfo_mod_only = true;
        });
        let m = resolve_mod(&mod_only, &src(0.0, 0.0, 0.5, 0.0, 0.0, 60));
        assert_eq!(m.pitch_mod, 0.0);
        assert!((m.pitch_mod_only - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_mod_only_sums_env_and_lfo() {
        // Both mod-only switches on: the channel carries env + LFO contributions
        // summed; pitch wheel still rides the common channel.
        let ctx = ctx_with(|c| {
            c.pitch_lfo_sel = LfoSel::Lfo1;
            c.pitch_lfo_depth = 2.0; // 0.5 → +1
            c.pitch_lfo_mod_only = true;
            c.pitch_env_sel = EnvSel::Env1;
            c.pitch_env_depth = 4.0; // 0.5 → +2
            c.pitch_env_mod_only = true;
            c.pitch_extra = 0.25;
        });
        let m = resolve_mod(&ctx, &src(0.5, 0.0, 0.5, 0.0, 0.0, 60));
        assert!((m.pitch_mod_only - 3.0).abs() < 1e-6);
        assert!((m.pitch_mod - 0.25).abs() < 1e-6); // wheel only
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
        assert_eq!(
            resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 60)).cutoff_mod,
            0.0
        );
        assert_eq!(
            resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 72)).cutoff_mod,
            12.0
        );
        assert_eq!(
            resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 48)).cutoff_mod,
            -12.0
        );
    }

    #[test]
    fn keytrack_off_ignores_note() {
        let ctx = neutral_ctx(); // key-track off
        assert_eq!(
            resolve_mod(&ctx, &src(0.0, 0.0, 0.0, 0.0, 0.0, 96)).cutoff_mod,
            0.0
        );
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
        assert_eq!(m.sweep_mod, 0.0);
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
        assert!(envelopes_static(
            &[false; N],
            &[true; N],
            &[true; N],
            &e1,
            &e2
        ));
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
        assert!(!envelopes_static(
            &[false; N],
            &[true; N],
            &[true; N],
            &e1,
            &e2
        ));
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
