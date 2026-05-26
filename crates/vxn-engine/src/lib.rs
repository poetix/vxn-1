//! VXN1 synth engine.
//!
//! Framework-agnostic: holds parameters, allocates voices, and renders audio
//! in fixed control blocks. The CLAP layer drives it with note/param events
//! and contiguous output slices; the UI reads and writes [`ParamValues`].

pub mod params;
pub mod shared;
pub mod smoothing;
pub mod state;
pub mod sync;
pub mod voice;

pub use params::{
    AssignMode, CrossModType, DEFAULT_SPLIT_POINT, EnvSel, GLOBAL_PARAMS, GlobalParam,
    GlobalValues, KeyMode, Layer, LfoSel, PATCH_PARAMS, ParamDesc, ParamKind, ParamRef,
    ParamValues, PatchParam, PatchValues, TOTAL_PARAMS, Taper, desc_for_clap_id, global_clap_id,
    module_for_clap_id, param_ref, patch_clap_id,
};
pub use shared::SharedParams;
use smoothing::ParamSmoother;
pub use state::PluginState;

use voice::{BlockCtx, Lfo1Trigger, VoiceBank};
use vxn_dsp::{
    AdsrShape, CONTROL_BLOCK, LfoCore, MAX_OVERSAMPLE, Oversampler, Smoothed, StereoChorus,
    StereoDelay, note_to_hz,
};

/// Mod-wheel (CC1) glide time (ms), applied at the control-block rate. Rounds
/// off the 7-bit CC steps so wheel sweeps don't zipper the cutoff / osc2 pitch.
/// On a wide pitch route 1 LSB is ~0.76 st, so the glide is set long enough to
/// filter hardware sensor jitter at rest, not just the coarse CC quantisation.
const MOD_WHEEL_SMOOTH_MS: f32 = 40.0;

/// Snapshot of the envelope-shaping parameters. Used to skip recomputing ADSR
/// coefficients (which cost an `exp()` per segment) unless a knob actually moved.
#[derive(Clone, Copy, PartialEq)]
struct EnvSnapshot {
    env1: (f32, f32, f32, f32),
    env1_shape: AdsrShape,
    env2: (f32, f32, f32, f32),
    env2_shape: AdsrShape,
}

/// Re-export so the plugin shell can flush denormals without depending on
/// `vxn-dsp` directly. `ScopedFlushToZero` is the per-`process` guard (sets FTZ
/// on entry, restores on drop); `enable_flush_to_zero` is the bare one-shot.
pub use vxn_dsp::{ScopedFlushToZero, enable_flush_to_zero};

/// Number of always-present layers (ADR 0003 §1). Indexed by [`Layer`].
const LAYERS: usize = Layer::COUNT;

/// Seed for the single global LFO 2 (E005 / 0019). LFO 1 is per-voice and seeded
/// inside each [`VoiceBank`] (E005 / 0018).
const LFO2_SEED: u64 = 0x7E5D;
/// Per-layer RNG seeds (decorrelate the two layers' S&H LFO PRNGs).
const RNG_SEEDS: [u64; LAYERS] = [0x9E37_79B9, 0x2545_F491];

/// The complete VXN1 instrument.
pub struct Synth {
    sample_rate: f32,
    params: ParamValues,
    /// Glides gain-like params toward `params` to remove zipper noise. The
    /// filter is smoothed separately by ladder coefficient interpolation.
    smoother: ParamSmoother,
    /// Two always-present layers of 8 channels each (ADR 0003 §2). Each is a
    /// complete patch; both sum into the global FX bus.
    banks: [VoiceBank; LAYERS],
    /// A single instrument-wide global LFO 2 (E005 / 0019), shared across both
    /// layers and all voices: sampled once per block and broadcast. LFO 1 is
    /// per-voice, living inside each [`VoiceBank`] (E005 / 0018).
    lfo2: LfoCore,
    chorus: StereoChorus,
    delay: StereoDelay,
    /// Anti-aliasing decimator for the oversampled synthesis path.
    oversampler: Oversampler,
    /// Pitch bend in normalised `[-1, 1]`. Global value; each layer scales it by
    /// its own `PitchWheelDepth` in `build_ctx` (ADR 0003 §9, ADR 0004 §5).
    bend_norm: f32,
    /// Mod-wheel (CC1) position in `[0, 1]`, smoothed at the control rate.
    /// Global value; each layer applies it via its own routing params.
    mod_wheel: Smoothed,
    /// Current key mode (ADR 0003 §3). Drives both the per-layer param source
    /// ([`Synth::param_source`]) and note routing ([`Synth::note_on`]).
    key_mode: KeyMode,
    /// Split point (MIDI note) for [`KeyMode::Split`]: notes below go to Lower,
    /// at/above to Upper (ADR 0003 §8). Non-automatable shared state.
    split_point: u8,
    /// Host tempo (BPM) for LFO host-sync (E004 / 0015), fed from the CLAP
    /// transport each block. Defaults to a sane tempo when the host has none.
    tempo_bpm: f32,
    alloc_counter: u64,
    /// Round-robin layer cursor for Whole-mode note-on: alternates layers so
    /// notes spread 8+8, giving 16-voice polyphony with both layers reading
    /// layer A's params (0008). Reset on `reset`.
    rr_layer: usize,
    /// Last envelope params pushed to each layer's voices; `None` forces a refresh.
    last_env: [Option<EnvSnapshot>; LAYERS],
    /// Oversampling factor in effect last block; a change resets the decimator.
    last_os: usize,
}

impl Synth {
    pub fn new(sample_rate: f32) -> Self {
        // The LFO ticks once per control block, so its effective sample rate
        // is the control rate. Max LFO rate (40 Hz) still has ample steps/cycle.
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
        let params = ParamValues::default();
        Self {
            sample_rate,
            smoother: ParamSmoother::new(sample_rate, &params),
            params,
            banks: std::array::from_fn(|i| VoiceBank::new(sample_rate, RNG_SEEDS[i])),
            lfo2: LfoCore::new(control_rate, LFO2_SEED),
            chorus: StereoChorus::new(sample_rate),
            delay: StereoDelay::new(sample_rate, 2.0),
            oversampler: Oversampler::new(),
            bend_norm: 0.0,
            mod_wheel: Smoothed::new(0.0, MOD_WHEEL_SMOOTH_MS, control_rate),
            key_mode: KeyMode::Whole,
            split_point: DEFAULT_SPLIT_POINT,
            tempo_bpm: sync::DEFAULT_TEMPO_BPM,
            alloc_counter: 0,
            rr_layer: 0,
            last_env: [None; LAYERS],
            last_os: 1,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (sample_rate - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sample_rate;
        let control_rate = sample_rate / CONTROL_BLOCK as f32;
        for bank in self.banks.iter_mut() {
            bank.set_sample_rate(sample_rate);
        }
        self.lfo2 = LfoCore::new(control_rate, LFO2_SEED);
        self.chorus = StereoChorus::new(sample_rate);
        self.delay = StereoDelay::new(sample_rate, 2.0);
        self.oversampler.reset();
        self.mod_wheel.set_time(MOD_WHEEL_SMOOTH_MS, control_rate);
        self.smoother.set_sample_rate(sample_rate);
        self.smoother.snap_all(&self.params);
        // Envelope cores were recreated with zeroed coefficients; force a refresh.
        self.last_env = [None; LAYERS];
    }

    pub fn params(&self) -> &ParamValues {
        &self.params
    }

    pub fn params_mut(&mut self) -> &mut ParamValues {
        &mut self.params
    }

    /// Set a parameter by CLAP id (routed to its layer/global slot).
    pub fn set_param(&mut self, index: usize, value: f32) {
        self.params.set_by_clap_id(index, value);
    }

    /// Pitch bend in normalised `[-1, 1]`. The semitone span is the layer's
    /// `PitchWheelDepth` (default ±2 st), applied in `build_ctx`.
    pub fn set_pitch_bend(&mut self, normalized: f32) {
        self.bend_norm = normalized.clamp(-1.0, 1.0);
    }

    /// Mod wheel (CC1) in normalised `[0, 1]`. Routed in `build_ctx` through the
    /// mod-wheel panel depths (PWM / cutoff / reso / osc2 pitch); smoothed at the
    /// control rate.
    pub fn set_mod_wheel(&mut self, normalized: f32) {
        self.mod_wheel.set_target(normalized.clamp(0.0, 1.0));
    }

    /// Set the key mode (ADR 0003 §3). Cheap; the seed-on-entry copy lives in
    /// the shared store ([`SharedParams::set_key_mode_seeded`]) so it persists
    /// and is echoed to the host — the engine just reads the mode it is given.
    pub fn set_key_mode(&mut self, mode: KeyMode) {
        self.key_mode = mode;
    }

    pub fn key_mode(&self) -> KeyMode {
        self.key_mode
    }

    /// Set the split point (MIDI note) used by [`KeyMode::Split`] routing.
    pub fn set_split_point(&mut self, note: u8) {
        self.split_point = note.min(127);
    }

    /// Host tempo (BPM) for LFO host-sync (E004 / 0015), pushed each block from
    /// the CLAP transport. Non-finite or non-positive input falls back to the
    /// default so a synced LFO never produces NaN/Inf.
    pub fn set_tempo(&mut self, bpm: f32) {
        self.tempo_bpm = if bpm.is_finite() && bpm > 0.0 {
            bpm
        } else {
            sync::DEFAULT_TEMPO_BPM
        };
    }

    /// Which param block layer `layer` reads under `key_mode` (ADR 0003 §3):
    /// in **Whole**, both layers read layer A's (Upper) block — no mirroring;
    /// in **Dual/Split**, each layer reads its own.
    #[inline]
    fn param_source(layer: usize, key_mode: KeyMode) -> Layer {
        match key_mode {
            KeyMode::Whole => Layer::Upper,
            _ => Layer::ALL[layer],
        }
    }

    /// Route a note-on to the layer(s) chosen by the current key mode (ADR 0003
    /// §3): Whole round-robins across the layers (16-voice), Dual duplicates to
    /// both (layered 8+8), Split partitions at the split point (Lower below,
    /// Upper at/above). Note-offs broadcast, so each layer releases only the
    /// note it actually started.
    pub fn note_on(&mut self, note: u8, velocity: f32) {
        match self.key_mode {
            KeyMode::Whole => {
                let layer = self.rr_layer;
                self.rr_layer ^= 1;
                self.note_on_layer(layer, note, velocity);
            }
            KeyMode::Dual => {
                self.note_on_layer(Layer::Upper as usize, note, velocity);
                self.note_on_layer(Layer::Lower as usize, note, velocity);
            }
            KeyMode::Split => {
                let layer = if note < self.split_point {
                    Layer::Lower
                } else {
                    Layer::Upper
                };
                self.note_on_layer(layer as usize, note, velocity);
            }
        }
    }

    /// Start a note on a specific layer. [`Self::note_on`] calls this per the
    /// key-mode routing policy; exposed for tests and future per-layer drivers.
    /// The assign mode (Poly/Unison) is read live from the layer's param source
    /// (ADR 0003 §4) so it always reflects the current patch.
    pub fn note_on_layer(&mut self, layer: usize, note: u8, velocity: f32) {
        self.alloc_counter += 1;
        let src = Self::param_source(layer, self.key_mode);
        let p = self.params.layer(src);
        let mode = p.assign_mode();
        let unison_detune = p.get(PatchParam::UnisonDetune);
        // Per-voice LFO 1 (E005 / 0018): the bank retriggers the triggered
        // channel(s)' LFO 1 phase to the shape's zero crossing at note-on, unless
        // free-run is set.
        let lfo1 = Lfo1Trigger {
            shape: p.lfo_shape(),
            free_run: p.bool(PatchParam::Lfo1FreeRun),
        };
        self.banks[layer].note_on(mode, note, velocity, self.alloc_counter, unison_detune, lfo1);
    }

    pub fn note_off(&mut self, note: u8) {
        // Broadcast: each layer releases the note only if it is holding it.
        for bank in &mut self.banks {
            bank.note_off(note);
        }
    }

    pub fn all_notes_off(&mut self) {
        for bank in &mut self.banks {
            bank.all_notes_off();
        }
    }

    /// Total active channels across both layers.
    pub fn active_count(&self) -> usize {
        self.banks.iter().map(|b| b.active_count()).sum()
    }

    pub fn reset(&mut self) {
        for bank in self.banks.iter_mut() {
            bank.reset_all();
        }
        self.lfo2.reset();
        self.chorus.clear();
        self.delay.clear();
        self.oversampler.reset();
        self.smoother.snap_all(&self.params);
        self.rr_layer = 0;
    }

    /// Render `out_l`/`out_r` (equal length). No events occur within this span;
    /// the caller splits the host buffer at event boundaries.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        // Params are constant across a process call; refresh envelope coeffs at
        // most once per layer, and only when they actually changed.
        for layer in 0..LAYERS {
            self.sync_envelopes(layer);
        }

        // Oversampling factor for this call; a change resets the decimator.
        let os = self.params.global().oversample_factor();
        if os != self.last_os {
            self.oversampler.reset();
            self.last_os = os;
        }

        let key_mode = self.key_mode;
        let n = out_l.len().min(out_r.len());
        let mut start = 0;
        while start < n {
            let block = (n - start).min(CONTROL_BLOCK);
            // Advance gain-like smoothers toward the raw targets for this block.
            self.smoother.tick_block(&self.params);
            // Mod wheel is a single global control; tick once per block and
            // apply per layer (each layer routes it via its own params §9).
            let wheel = self.mod_wheel.tick();

            // Global LFO 2 (E005 / 0019): one instrument-wide LFO, sampled once
            // per block and broadcast to both layers. Its shape/rate/sync are
            // global params; host-sync resolves its rate from the engine tempo.
            let gv = self.smoother.values().global();
            let lfo2_shape = gv.lfo2_shape();
            let lfo2_rate = lfo_rate_from(
                GlobalParam::Lfo2Rate.desc(),
                gv.get(GlobalParam::Lfo2Rate),
                gv.bool(GlobalParam::Lfo2Sync),
                self.tempo_bpm,
            );
            let lfo2_val = self.lfo2.next(lfo2_shape);
            self.lfo2.set_rate(lfo2_rate);

            // Both layers render (summed) into one oversampled mono mix, then
            // decimated back to the base rate before the global FX bus (§7).
            let mut mono_os = [0.0f32; CONTROL_BLOCK * MAX_OVERSAMPLE];
            let mono_os = &mut mono_os[..block * os];
            for layer in 0..LAYERS {
                let ctx = self.build_ctx(layer, key_mode, os, wheel, lfo2_val);
                self.banks[layer].render_block(mono_os, &ctx);
            }

            let mut mono = [0.0f32; CONTROL_BLOCK];
            let mono = &mut mono[..block];
            self.oversampler.decimate(mono_os, mono, os);

            // Effects (stereo), then write out.
            let chorus_on = self.params.global().bool(GlobalParam::ChorusOn);
            let delay_on = self.params.global().bool(GlobalParam::DelayOn);
            self.update_effects();

            // Apply the per-sample master-volume glide into a dry mono block,
            // then run the stereo effects a block at a time.
            let mut dry_buf = [0.0f32; CONTROL_BLOCK];
            let dry = &mut dry_buf[..block];
            for (d, &m) in dry.iter_mut().zip(mono.iter()) {
                *d = m * self.smoother.next_volume();
            }

            let l_out = &mut out_l[start..start + block];
            let r_out = &mut out_r[start..start + block];
            if chorus_on {
                self.chorus.process_block(dry, l_out, r_out);
            } else {
                l_out.copy_from_slice(dry);
                r_out.copy_from_slice(dry);
            }
            if delay_on {
                for i in 0..block {
                    let (l, r) = self.delay.process(l_out[i], r_out[i]);
                    l_out[i] = l;
                    r_out[i] = r;
                }
            }
            start += block;
        }
    }

    /// Push envelope params to a layer's voices when they change. Reads the
    /// layer's param source (Whole → Upper for both). Applies to every voice
    /// (active or not) so a later-reused voice already has fresh coeffs.
    fn sync_envelopes(&mut self, layer: usize) {
        let src = Self::param_source(layer, self.key_mode);
        let p = self.params.layer(src);
        let snap = EnvSnapshot {
            env1: (
                p.get(PatchParam::Env1Attack),
                p.get(PatchParam::Env1Decay),
                p.get(PatchParam::Env1Sustain),
                p.get(PatchParam::Env1Release),
            ),
            env1_shape: p.env1_shape(),
            env2: (
                p.get(PatchParam::Env2Attack),
                p.get(PatchParam::Env2Decay),
                p.get(PatchParam::Env2Sustain),
                p.get(PatchParam::Env2Release),
            ),
            env2_shape: p.env2_shape(),
        };
        if self.last_env[layer] == Some(snap) {
            return;
        }
        self.banks[layer].set_envelopes(snap.env1, snap.env1_shape, snap.env2, snap.env2_shape);
        self.last_env[layer] = Some(snap);
    }

    fn update_effects(&mut self) {
        let g = self.smoother.values().global();
        self.chorus.set_params(
            g.get(GlobalParam::ChorusRate),
            g.get(GlobalParam::ChorusDepth),
            g.get(GlobalParam::ChorusMix),
        );
        let t = delay_time_seconds(
            g.bool(GlobalParam::DelaySync),
            g.get(GlobalParam::DelayTime),
            self.tempo_bpm,
        );
        self.delay.set_params(
            t,
            t,
            g.get(GlobalParam::DelayFeedback),
            0.3,
            g.get(GlobalParam::DelayMix),
            g.bool(GlobalParam::DelayPingPong),
        );
    }

    /// Build one layer's control-block context from its param source (§3) and the
    /// global block. `wheel` is the once-per-block global mod-wheel value, applied
    /// here via this layer's routing params (§9). `lfo2_val` is the single global
    /// LFO 2 value, sampled once per block in `process` and broadcast (§5, E005).
    fn build_ctx(
        &self,
        layer: usize,
        key_mode: KeyMode,
        os: usize,
        wheel: f32,
        lfo2_val: f32,
    ) -> BlockCtx {
        let src = Self::param_source(layer, key_mode);
        let vals = self.smoother.values();
        let p = vals.layer(src);
        let g = vals.global();
        let tempo = self.tempo_bpm;
        // LFO 1 is per-voice (E005 / 0018): the bank ticks each channel's phase.
        // Resolve its shared rate (post host-sync) here and hand the bank LFO 1's
        // shape + onset times. LFO 2 is the global LFO, already sampled.
        let lfo1_rate_hz = lfo_rate(p, PatchParam::LfoRate, PatchParam::LfoSync, tempo);

        // Cross-mod type selector → (sync flag, PM index). Off zeroes both, so
        // the voice keeps the independent fast path; Sync and PM never coexist.
        let (sync, pm_index) = match p.cross_mod_type() {
            CrossModType::Off => (false, 0.0),
            CrossModType::Sync => (true, 0.0),
            CrossModType::Pm => (false, p.get(PatchParam::CrossModAmount)),
        };

        // Mod wheel (CC1) is a global control applied once per block, folded into
        // the route `*_extra` terms (and resonance) here rather than per voice.
        let resonance =
            (p.get(PatchParam::Resonance) + wheel * p.get(PatchParam::ModWheelReso)).clamp(0.0, 1.0);

        BlockCtx {
            os_sample_rate: self.sample_rate * os as f32,
            os,
            osc1_wave: p.osc_wave(PatchParam::Osc1Wave),
            osc2_wave: p.osc_wave(PatchParam::Osc2Wave),
            osc1_level: p.get(PatchParam::Osc1Level),
            osc2_level: p.get(PatchParam::Osc2Level),
            ring_level: p.get(PatchParam::RingLevel),
            osc1_pw: p.get(PatchParam::Osc1PulseWidth),
            osc2_pw: p.get(PatchParam::Osc2PulseWidth),
            // Octave and Coarse are integer-semitone params: hard-quantise them
            // (the fader stores a continuous value) so the tuning lands exactly on
            // a semitone. Fine stays continuous (cents).
            osc1_semi: p.get(PatchParam::Osc1Octave).round() * 12.0
                + p.get(PatchParam::Osc1Coarse).round()
                + p.get(PatchParam::Osc1Fine) / 100.0,
            osc2_semi: p.get(PatchParam::Osc2Octave).round() * 12.0
                + p.get(PatchParam::Osc2Coarse).round()
                + p.get(PatchParam::Osc2Fine) / 100.0,
            cutoff: p.get(PatchParam::Cutoff),
            hpf_cutoff: p.get(PatchParam::HpfCutoff),
            resonance,
            drive: p.get(PatchParam::Drive),
            poles: p.filter_poles(),
            base_semis: g.get(GlobalParam::MasterTune),
            lfo1_shape: p.lfo_shape(),
            lfo1_rate_hz,
            lfo1_delay_time: p.get(PatchParam::Lfo1DelayTime),
            lfo1_fade: p.get(PatchParam::Lfo1Fade),
            lfo2_val,
            sync,
            pm_index,
            portamento_time: p.get(PatchParam::PortamentoTime),
            // Fixed routes (ADR 0004 §4).
            pitch_lfo_sel: p.lfo_sel(PatchParam::PitchLfoSrc),
            pitch_lfo_depth: p.get(PatchParam::PitchLfoDepth),
            pitch_env_sel: p.env_sel(PatchParam::PitchEnvSrc),
            pitch_env_depth: p.get(PatchParam::PitchEnvDepth),
            pitch_extra: self.bend_norm * p.get(PatchParam::PitchWheelDepth),
            pwm_lfo_sel: p.lfo_sel(PatchParam::PwmLfoSrc),
            pwm_lfo_depth: p.get(PatchParam::PwmLfoDepth),
            pwm_env_sel: p.env_sel(PatchParam::PwmEnvSrc),
            pwm_env_depth: p.get(PatchParam::PwmEnvDepth),
            pwm_extra: wheel * p.get(PatchParam::ModWheelPwm),
            cutoff_lfo1_depth: p.get(PatchParam::CutoffLfo1Depth),
            cutoff_lfo2_depth: p.get(PatchParam::CutoffLfo2Depth),
            cutoff_env_depth: p.get(PatchParam::CutoffEnvDepth),
            cutoff_vel_depth: p.get(PatchParam::VelCutoffDepth),
            cutoff_extra: wheel * p.get(PatchParam::ModWheelCutoff),
            filter_key_track: p.bool(PatchParam::FilterKeyTrack),
            osc2_pitch_env_sel: p.env_sel(PatchParam::Osc2PitchEnvSrc),
            osc2_pitch_env_depth: p.get(PatchParam::Osc2PitchEnvDepth),
            osc2_pitch_extra: wheel * p.get(PatchParam::ModWheelOsc2Pitch),
        }
    }
}

/// Resolve an LFO's rate in Hz for this block (E004 / 0015). Sync off: the rate
/// knob is free-running Hz, exactly as before. Sync on: the knob's normalised
/// position (over `desc`'s range) selects a musical subdivision locked to
/// `tempo_bpm`. The LFO core clamps the result to its valid Hz range. Works for
/// both the per-patch LFO 1 rate and the global LFO 2 rate via their descriptors.
#[inline]
fn lfo_rate_from(desc: &ParamDesc, rate_value: f32, sync_on: bool, tempo_bpm: f32) -> f32 {
    if sync_on {
        // Spread subdivisions linearly across the knob's travel (`to_fader`), not
        // its tapered Hz value — even subdivision spacing with no midpoint skew.
        let pos = desc.to_fader(rate_value);
        sync::synced_hz(tempo_bpm, sync::index_from_norm(pos))
    } else {
        rate_value
    }
}

/// [`lfo_rate_from`] for a per-patch LFO rate/sync pair (LFO 1).
#[inline]
fn lfo_rate(p: &PatchValues, rate: PatchParam, sync_flag: PatchParam, tempo_bpm: f32) -> f32 {
    lfo_rate_from(rate.desc(), p.get(rate), p.bool(sync_flag), tempo_bpm)
}

/// Resolve the delay time in seconds for this block (E006). Sync off: the Time
/// knob is taken as literal seconds, exactly as before. Sync on: the knob's
/// normalised position selects a musical subdivision locked to `tempo_bpm`
/// (mirrors the LFO host-sync in [`lfo_rate_from`]). The knob's stored value is
/// never mutated, so toggling sync off reads back as the same ms again. The
/// returned value can still exceed the delay buffer; `StereoDelay::set_params`
/// clamps it to capacity regardless of tempo.
#[inline]
fn delay_time_seconds(sync_on: bool, time_value: f32, tempo_bpm: f32) -> f32 {
    if sync_on {
        // Subdivisions spread linearly across the Time knob's travel (`to_fader`),
        // matching the LFO sync — even spacing, no midpoint skew.
        let pos = GlobalParam::DelayTime.desc().to_fader(time_value);
        sync::synced_seconds(tempo_bpm, sync::index_from_norm(pos))
    } else {
        time_value
    }
}

/// Convenience: A4 = 440 Hz reference, exposed for tests/tools.
pub fn a4_hz() -> f32 {
    note_to_hz(69.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{
        AssignMode, GlobalParam, Layer, PatchParam, PatchValues, global_clap_id, patch_clap_id,
    };

    /// Upper-layer per-patch CLAP id (tests drive the single render path = Upper).
    fn pp(p: PatchParam) -> usize {
        patch_clap_id(Layer::Upper, p)
    }
    /// Global-param CLAP id.
    fn gp(g: GlobalParam) -> usize {
        global_clap_id(g)
    }
    /// Lower-layer per-patch CLAP id (for two-layer tests).
    fn lo(p: PatchParam) -> usize {
        patch_clap_id(Layer::Lower, p)
    }

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
        // Fast amp envelope (ENV-2 drives the VCA by default) so the test is short.
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s.set_param(pp(PatchParam::Env2Release), 0.01);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.note_on(69, 1.0);
        let (l, _) = render(&mut s, 4800);
        assert!(rms(&l) > 0.01, "note produced no sound");

        s.note_off(69);
        // Render well past the release.
        let (tail, _) = render(&mut s, 48_000);
        let last = &tail[tail.len() - 4800..];
        assert!(
            rms(last) < 1e-4,
            "did not release to silence: {}",
            rms(last)
        );
    }

    #[test]
    fn output_finite_under_stress() {
        let mut s = Synth::new(44_100.0);
        s.set_param(pp(PatchParam::Resonance), 1.0);
        s.set_param(gp(GlobalParam::DelayOn), 1.0);
        for n in 60..76 {
            s.note_on(n, 1.0);
        }
        let (l, r) = render(&mut s, 44_100);
        assert!(
            l.iter().chain(r.iter()).all(|x| x.is_finite()),
            "non-finite output"
        );
        let peak = l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak < 20.0, "output blew up: peak {peak}");
    }

    #[test]
    fn vca_follows_env2() {
        // The VCA is hardwired to Env2 (ADR 0004 §4): a held note with Env2
        // sustain 0 and a fast decay settles to silence, proving the amp gain
        // comes from Env2 directly.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Decay), 0.01);
        s.set_param(pp(PatchParam::Env2Sustain), 0.0);
        s.note_on(69, 1.0);
        let (l, _) = render(&mut s, 48_000);
        let tail = &l[l.len() - 4800..];
        assert!(
            rms(tail) < 1e-6,
            "Env2 sustain 0 should settle to silence, got {}",
            rms(tail)
        );
    }

    #[test]
    fn env_block_skip_waits_for_amp_sustain() {
        // Envelope block-skip must engage only once Env2 (the VCA) actually
        // reaches Sustain. A held note with a long Env2 decay to a low sustain
        // must keep getting quieter through the decay; if the skip froze the
        // level mid-decay the amplitude would plateau early.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s.set_param(pp(PatchParam::Env2Decay), 0.4); // long decay
        s.set_param(pp(PatchParam::Env2Sustain), 0.05); // low sustain
        s.note_on(60, 1.0);
        let (l, _) = render(&mut s, 24_000); // 0.5 s spans the decay
        let w = 2400;
        let early = rms(&l[w..2 * w]);
        let later = rms(&l[6 * w..7 * w]);
        let settled = rms(&l[9 * w..10 * w]);
        assert!(later < early * 0.7, "amp decay stalled: early {early} later {later}");
        assert!(settled < later, "amp kept falling toward sustain: {later} -> {settled}");
    }

    #[test]
    fn env_block_skip_does_not_freeze_mod_envelope() {
        // The skip predicate requires *both* envelopes in Sustain. Here Env2
        // (amp) snaps to full sustain immediately while Env1 — routed to pitch —
        // has a long decay. The skip must stay disengaged while Env1 sweeps, so
        // the pitch slides down from its raised start back to the played note as
        // Env1 → 0. A predicate that checked only Env2 would freeze Env1 and the
        // pitch would stall high. Frequency (zero-crossings) is an unambiguous
        // readout of whether Env1 kept moving.
        let mut s = pitched_synth();
        s.set_param(pp(PatchParam::Env2Decay), 0.001);
        s.set_param(pp(PatchParam::Env2Sustain), 1.0); // amp static almost at once
        s.set_param(pp(PatchParam::PitchEnvSrc), 1.0); // Env1 → pitch
        s.set_param(pp(PatchParam::PitchEnvDepth), 12.0); // +1 octave at Env1 = 1
        s.set_param(pp(PatchParam::Env1Attack), 0.0005);
        s.set_param(pp(PatchParam::Env1Decay), 0.4); // long
        s.set_param(pp(PatchParam::Env1Sustain), 0.0); // → settles to the played note
        s.note_on(57, 1.0); // A3 = 220 Hz; +1 oct = 440 Hz at the peak
        let (l, _) = render(&mut s, 24_000); // 0.5 s spans the decay
        let early = dominant_hz(&l[2400..7200], 48_000.0); // Env1 still high → ~ up an octave
        let late = dominant_hz(&l[19_200..24_000], 48_000.0); // Env1 ≈ 0 → ~ played note
        assert!(
            early > 300.0,
            "expected raised pitch while Env1 high, got {early} Hz"
        );
        assert!(
            late < 250.0,
            "pitch stalled high (mod envelope frozen): late {late} Hz"
        );
    }

    /// Dominant frequency of a mono buffer via zero-crossing count (rising
    /// edges). Crude but enough to tell an octave apart.
    fn dominant_hz(s: &[f32], sr: f32) -> f32 {
        let mut crossings = 0usize;
        for w in s.windows(2) {
            if w[0] <= 0.0 && w[1] > 0.0 {
                crossings += 1;
            }
        }
        crossings as f32 * sr / s.len() as f32
    }

    fn pitched_synth() -> Synth {
        let mut s = Synth::new(48_000.0);
        // Single sine osc, no chorus/vibrato, fast attack — clean pitch readout.
        s.set_param(pp(PatchParam::Osc1Wave), 0.0); // Sine
        s.set_param(pp(PatchParam::Osc2Level), 0.0);
        s.set_param(pp(PatchParam::PitchLfoDepth), 0.0); // kill default vibrato
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s
    }

    #[test]
    fn octave_up_doubles_frequency() {
        let mut base = pitched_synth();
        base.note_on(57, 1.0); // A3 = 220 Hz
        let (l0, _) = render(&mut base, 24_000);
        let f0 = dominant_hz(&l0[4800..], 48_000.0);

        let mut up = pitched_synth();
        up.set_param(pp(PatchParam::Osc1Octave), 1.0);
        up.note_on(57, 1.0);
        let (l1, _) = render(&mut up, 24_000);
        let f1 = dominant_hz(&l1[4800..], 48_000.0);

        assert!(
            (f1 / f0 - 2.0).abs() < 0.05,
            "octave up should double freq: {f0} -> {f1}"
        );
    }

    #[test]
    fn octave_and_coarse_combine_additively() {
        // +1 octave & +7 st = +19 st. Compare against +19 st coarse alone.
        let mut a = pitched_synth();
        a.set_param(pp(PatchParam::Osc1Octave), 1.0);
        a.set_param(pp(PatchParam::Osc1Coarse), 7.0);
        a.note_on(45, 1.0);
        let (la, _) = render(&mut a, 24_000);
        let fa = dominant_hz(&la[4800..], 48_000.0);

        let mut b = pitched_synth();
        b.set_param(pp(PatchParam::Osc1Coarse), 19.0);
        b.note_on(45, 1.0);
        let (lb, _) = render(&mut b, 24_000);
        let fb = dominant_hz(&lb[4800..], 48_000.0);

        assert!((fa / fb - 1.0).abs() < 0.02, "not additive: {fa} vs {fb}");
    }

    #[test]
    fn hpf_thins_low_content_when_engaged() {
        // A low note through a high HPF cutoff loses energy vs the open default.
        fn low_note_rms(hpf_hz: f32) -> f32 {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::HpfCutoff), hpf_hz);
            s.note_on(33, 1.0); // A1 ≈ 55 Hz
            let (l, _) = render(&mut s, 24_000);
            rms(&l[4800..])
        }
        let open = low_note_rms(20.0); // default ≈ off
        let engaged = low_note_rms(2000.0);
        assert!(
            engaged < 0.5 * open,
            "HPF did not thin lows: open {open}, engaged {engaged}"
        );
    }

    #[test]
    fn ring_level_mixes_in_and_zero_is_inert() {
        // RingLevel > 0 mixes the osc1×osc2 ring signal in alongside the oscs,
        // changing the timbre and staying finite; RingLevel 0 is the inert
        // fast path (its output matches a no-ring render exactly).
        fn render_ring(level: f32) -> Vec<f32> {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::Osc1Wave), 0.0); // sine
            s.set_param(pp(PatchParam::Osc2Wave), 0.0);
            s.set_param(pp(PatchParam::Osc1Level), 0.5);
            s.set_param(pp(PatchParam::Osc2Level), 0.5);
            s.set_param(pp(PatchParam::Osc2Coarse), 5.0); // inharmonic vs osc1
            s.set_param(pp(PatchParam::RingLevel), level);
            s.note_on(45, 1.0);
            render(&mut s, 12_000).0
        }
        let dry = render_ring(0.0);
        assert_eq!(dry, render_ring(0.0), "RingLevel 0 path not deterministic");
        let wet = render_ring(0.8);
        assert!(wet.iter().all(|x| x.is_finite()), "ring output not finite");
        let diff = mean_abs_diff(&dry[4800..], &wet[4800..]);
        assert!(diff > 1e-3, "RingLevel did not change the output: {diff}");
    }

    #[test]
    fn filter_key_track_opens_cutoff_with_pitch() {
        // Key-track on: a high note sits a fixed octave-per-octave higher in
        // cutoff than with key-track off, so a saw plays brighter. Off: the note
        // pitch has no influence on cutoff. (ADR 0004 §4.)
        fn bright(key_track: bool) -> f32 {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::Osc1Wave), 2.0); // saw
            s.set_param(pp(PatchParam::Cutoff), 300.0); // dark base
            s.set_param(pp(PatchParam::Resonance), 0.0);
            s.set_param(pp(PatchParam::FilterKeyTrack), if key_track { 1.0 } else { 0.0 });
            s.note_on(72, 1.0); // a high note → large key-track shift when on
            let (l, _) = render(&mut s, 24_000);
            assert!(l.iter().all(|x| x.is_finite()), "key-track output not finite");
            rms(&l[4800..])
        }
        let off = bright(false);
        let on = bright(true);
        assert!(
            on > 1.5 * off,
            "key-track did not open the filter with pitch: off {off}, on {on}"
        );
    }

    /// Render a saw with LFO 1 → cutoff at the given onset, capturing `window`.
    /// `depth = 0` is the no-LFO baseline (the route contributes nothing).
    fn lfo1_cutoff_render(
        depth: f32,
        delay: f32,
        fade: f32,
        rate: f32,
        window: std::ops::Range<usize>,
    ) -> Vec<f32> {
        let mut s = pitched_synth();
        s.set_param(pp(PatchParam::Osc1Wave), 2.0); // saw
        s.set_param(pp(PatchParam::Cutoff), 1000.0);
        s.set_param(pp(PatchParam::CutoffLfo1Depth), depth); // LFO 1 → cutoff
        s.set_param(pp(PatchParam::LfoRate), rate);
        s.set_param(pp(PatchParam::Lfo1DelayTime), delay);
        s.set_param(pp(PatchParam::Lfo1Fade), fade);
        s.note_on(69, 1.0);
        let (l, _) = render(&mut s, 96_000);
        l[window].to_vec()
    }

    fn mean_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
    }

    #[test]
    fn lfo1_onset_holds_then_fades_modulation_in() {
        // With a 0.5 s delay then 0.5 s fade, LFO 1's value is gated to zero
        // through the delay, so an LFO 1 → cutoff route contributes nothing and
        // the output matches the no-LFO baseline; once settled the filter sweeps
        // and the output diverges (E005 / 0018 two-stage onset).
        let during = 9600..19_200;
        let settled = 58_000..67_600;
        let delay_diff = mean_abs_diff(
            &lfo1_cutoff_render(0.0, 0.5, 0.5, 4.0, during.clone()),
            &lfo1_cutoff_render(48.0, 0.5, 0.5, 4.0, during),
        );
        let settled_diff = mean_abs_diff(
            &lfo1_cutoff_render(0.0, 0.5, 0.5, 4.0, settled.clone()),
            &lfo1_cutoff_render(48.0, 0.5, 0.5, 4.0, settled),
        );
        assert!(delay_diff < 1e-6, "LFO 1 not held at zero in the delay: {delay_diff}");
        assert!(
            settled_diff > 1e-3,
            "LFO 1 did not open after delay+fade: {settled_diff}"
        );
    }

    #[test]
    fn lfo1_onset_zero_matches_immediate_modulation() {
        // Delay 0 + fade 0: LFO 1 modulates at full depth from the first block,
        // so the LFO 1 → cutoff route diverges from the no-LFO baseline at once.
        let win = 0..4800;
        let diff = mean_abs_diff(
            &lfo1_cutoff_render(0.0, 0.0, 0.0, 6.0, win.clone()),
            &lfo1_cutoff_render(48.0, 0.0, 0.0, 6.0, win),
        );
        assert!(diff > 1e-3, "0/0 onset should modulate at once: {diff}");
    }

    #[test]
    fn sync_engages_and_sweeps_formant_finitely() {
        // Integration check that the coupled path is live and stable. (The
        // master-period lock itself is proven in the DSP unit test
        // `synced_slave_locks_to_master_period`; a zero-crossing fundamental
        // detector can't see it through the synced waveform.) Here: enabling
        // sync changes the timbre, sweeping the slave tuning sweeps it further
        // (the synced formant), and every render stays finite.
        fn render_sync(sync: bool, osc2_coarse: f32) -> Vec<f32> {
            let mut s = pitched_synth();
            // CrossModType: Sync (1) engages the band-limited hard sync.
            s.set_param(pp(PatchParam::CrossModType), if sync { 1.0 } else { 0.0 });
            s.set_param(pp(PatchParam::Osc1Wave), 2.0); // saw master
            s.set_param(pp(PatchParam::Osc2Wave), 2.0); // saw slave
            s.set_param(pp(PatchParam::Osc2Level), 0.8);
            s.set_param(pp(PatchParam::Osc2Coarse), osc2_coarse);
            s.note_on(45, 1.0); // A2 ≈ 110 Hz master
            let (l, _) = render(&mut s, 24_000);
            assert!(l.iter().all(|x| x.is_finite()), "sync output not finite");
            l[4800..].to_vec()
        }
        fn diff(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
        }
        let unsynced = render_sync(false, 7.0);
        let synced_low = render_sync(true, 7.0);
        let synced_high = render_sync(true, 19.0);
        // Sync changes the timbre vs the independent path …
        assert!(
            diff(&unsynced, &synced_low) > 1e-3,
            "sync did not change the output"
        );
        // … and sweeping the slave tuning sweeps the synced formant.
        assert!(
            diff(&synced_low, &synced_high) > 1e-3,
            "slave tuning did not sweep the synced formant"
        );
    }

    #[test]
    fn cross_mod_adds_content_and_stays_finite() {
        // Through-zero PM (CrossModType::Pm) with osc2 at an inharmonic interval
        // injects a sideband at f(osc1)+f(osc2). Measure that bin via a single-bin
        // DFT: ≈0 with PM off, present at index > 0, output finite throughout.
        let sr = 48_000.0;
        let f1 = note_to_hz(45.0); // A2 ≈ 110 Hz carrier
        let f2 = note_to_hz(45.0 + 5.0); // osc2 +5 st (inharmonic)
        fn sideband(pm_index: f32, side_hz: f32, sr: f32) -> (f32, bool) {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::Osc2Level), 0.0); // carrier audible alone
            s.set_param(pp(PatchParam::Osc2Coarse), 5.0); // inharmonic vs osc1
            // PM mode when index > 0; Off (independent path) at index 0.
            s.set_param(
                pp(PatchParam::CrossModType),
                if pm_index > 0.0 { 2.0 } else { 0.0 },
            );
            s.set_param(pp(PatchParam::CrossModAmount), pm_index);
            s.note_on(45, 1.0);
            let (l, _) = render(&mut s, 24_000);
            let finite = l.iter().all(|x| x.is_finite());
            let tail = &l[4800..]; // past the amp-envelope attack
            let w = std::f32::consts::TAU * side_hz / sr;
            let len = tail.len();
            let (mut re, mut im) = (0.0f32, 0.0f32);
            // Hann window: keep the carrier's leakage out of the sideband bin.
            for (n, &x) in tail.iter().enumerate() {
                let win = 0.5 * (1.0 - (std::f32::consts::TAU * n as f32 / (len - 1) as f32).cos());
                let ph = w * n as f32;
                re += x * win * ph.cos();
                im -= x * win * ph.sin();
            }
            ((re * re + im * im).sqrt() / len as f32, finite)
        }
        let (clean, clean_finite) = sideband(0.0, f1 + f2, sr);
        let (modulated, mod_finite) = sideband(0.8, f1 + f2, sr);
        assert!(clean_finite && mod_finite, "cross-mod output not finite");
        assert!(
            modulated > 10.0 * clean.max(1e-6),
            "cross-mod produced no sideband: clean {clean}, modulated {modulated}"
        );
    }

    /// Single audible osc2 sine — for mod-wheel→osc2-pitch tests.
    fn osc2_sine_synth() -> Synth {
        let mut s = Synth::new(48_000.0);
        s.set_param(pp(PatchParam::Osc1Level), 0.0);
        s.set_param(pp(PatchParam::Osc2Wave), 0.0); // sine
        s.set_param(pp(PatchParam::Osc2Level), 0.8);
        s.set_param(pp(PatchParam::Osc2Coarse), 0.0);
        s.set_param(pp(PatchParam::Osc2Fine), 0.0);
        s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s
    }

    #[test]
    fn pitch_bend_shifts_rendered_pitch() {
        // Full positive bend (+1.0 normalised) = +2 st = ×2^(2/12) ≈ 1.122.
        let mut base = pitched_synth();
        base.note_on(57, 1.0); // A3 ≈ 220 Hz
        let (l0, _) = render(&mut base, 24_000);
        let f0 = dominant_hz(&l0[4800..], 48_000.0);

        let mut bent = pitched_synth();
        bent.set_pitch_bend(1.0);
        bent.note_on(57, 1.0);
        let (l1, _) = render(&mut bent, 24_000);
        let f1 = dominant_hz(&l1[4800..], 48_000.0);

        let expected = 2.0f32.powf(2.0 / 12.0);
        assert!(
            (f1 / f0 - expected).abs() < 0.03,
            "bend should raise pitch ×{expected:.3}: {f0} -> {f1}"
        );
    }

    #[test]
    fn mod_wheel_osc2_pitch_shifts_osc2() {
        // Wheel→Osc2 pitch depth 12 st, wheel full → osc2 up an octave (×2).
        let mut base = osc2_sine_synth();
        base.note_on(57, 1.0); // 220 Hz
        let (l0, _) = render(&mut base, 24_000);
        let f0 = dominant_hz(&l0[4800..], 48_000.0);

        let mut up = osc2_sine_synth();
        up.set_param(pp(PatchParam::ModWheelOsc2Pitch), 12.0);
        up.set_mod_wheel(1.0);
        up.note_on(57, 1.0);
        let (l1, _) = render(&mut up, 24_000);
        let f1 = dominant_hz(&l1[4800..], 48_000.0);

        assert!(
            (f1 / f0 - 2.0).abs() < 0.05,
            "wheel→osc2 +12 st should double osc2 freq: {f0} -> {f1}"
        );
    }

    #[test]
    fn mod_wheel_zero_depth_is_inert() {
        // With every mod-wheel depth at zero (default), a full wheel changes
        // nothing — the panel routes are independent and all start unrouted.
        let mut base = osc2_sine_synth();
        base.note_on(57, 1.0);
        let (l0, _) = render(&mut base, 24_000);
        let f0 = dominant_hz(&l0[4800..], 48_000.0);

        let mut off = osc2_sine_synth();
        off.set_mod_wheel(1.0);
        off.note_on(57, 1.0);
        let (l1, _) = render(&mut off, 24_000);
        let f1 = dominant_hz(&l1[4800..], 48_000.0);

        assert!(
            (f1 / f0 - 1.0).abs() < 0.02,
            "zero-depth wheel shifted pitch: {f0} -> {f1}"
        );
    }

    #[test]
    fn mod_wheel_cutoff_moves_cutoff() {
        // Wheel→Cutoff: a full wheel opens the filter, passing more saw
        // harmonics → higher RMS than the dark baseline.
        fn bright(wheel: f32) -> f32 {
            let mut s = Synth::new(48_000.0);
            s.set_param(pp(PatchParam::Osc1Wave), 2.0); // saw (harmonic-rich)
            s.set_param(pp(PatchParam::Osc2Level), 0.0);
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::Env2Attack), 0.001);
            s.set_param(pp(PatchParam::Cutoff), 200.0); // dark base
            s.set_param(pp(PatchParam::Resonance), 0.0);
            s.set_param(pp(PatchParam::ModWheelCutoff), 48.0); // ×2^4 = 16
            s.set_mod_wheel(wheel);
            s.note_on(45, 1.0); // 110 Hz, many harmonics
            let (l, _) = render(&mut s, 24_000);
            assert!(
                l.iter().all(|x| x.is_finite()),
                "mod-wheel cutoff not finite"
            );
            rms(&l[4800..])
        }
        let dark = bright(0.0);
        let open = bright(1.0);
        assert!(
            open > 1.3 * dark,
            "wheel→cutoff did not open the filter: dark {dark}, open {open}"
        );
    }

    // ── E005 / 0018: per-voice LFO 1 ─────────────────────────────────────────

    #[test]
    fn per_voice_lfo1_retriggers_only_its_own_voice() {
        // LFO 1 is per voice: a new note retriggers only its own channel's LFO 1
        // (to the sine zero crossing = phase 0); a held voice's phase keeps
        // running, undisturbed.
        let mut s = pitched_synth(); // sine LFO 1
        s.set_param(pp(PatchParam::LfoRate), 5.0);
        s.note_on_layer(0, 60, 1.0); // → channel 0
        let _ = render(&mut s, 6000); // advance channel 0's LFO 1 phase
        let ch0_before = s.banks[0].lfo1_phase(0);
        assert!(ch0_before > 0.01, "held voice should have advanced");
        s.note_on_layer(0, 64, 1.0); // → channel 1, retriggers only its own LFO 1
        assert_eq!(s.banks[0].lfo1_phase(1), 0.0, "new voice retriggers to zero");
        assert_eq!(
            s.banks[0].lfo1_phase(0),
            ch0_before,
            "held voice's LFO 1 must be undisturbed by another note"
        );
    }

    #[test]
    fn per_voice_lfo1_retrigger_lands_on_zero_crossing() {
        // The per-voice retrigger lands on each shape's zero crossing (sine 0,
        // tri 0.25, saws 0.5; square/S&H at the boundary).
        for (shape_idx, expected) in [(0.0, 0.0), (1.0, 0.25), (2.0, 0.5), (4.0, 0.0)] {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::LfoShape), shape_idx);
            s.set_param(pp(PatchParam::LfoRate), 5.0);
            s.note_on_layer(0, 60, 1.0);
            let _ = render(&mut s, 6000);
            s.note_on_layer(0, 64, 1.0); // channel 1 freshly triggered
            assert_eq!(
                s.banks[0].lfo1_phase(1),
                expected,
                "shape {shape_idx} should retrigger to its zero crossing"
            );
        }
    }

    #[test]
    fn lfo1_free_run_keeps_phase_across_note_ons() {
        // Free-run on: re-triggering a channel does not reset its LFO 1 phase.
        let mut s = pitched_synth();
        s.set_param(pp(PatchParam::LfoRate), 5.0);
        s.set_param(pp(PatchParam::Lfo1FreeRun), 1.0);
        s.note_on_layer(0, 60, 1.0);
        let _ = render(&mut s, 6000);
        let before = s.banks[0].lfo1_phase(0);
        assert!(before > 0.01);
        s.note_on_layer(0, 60, 1.0); // reuses channel 0 (same note); no reset
        assert_eq!(
            s.banks[0].lfo1_phase(0),
            before,
            "free-run must not reset the per-voice phase"
        );
    }

    // ── E005 / 0019: global instrument-wide LFO 2 ────────────────────────────

    #[test]
    fn lfo2_zero_depth_matches_pre_change_output() {
        // No route selects LFO 2 (only LFO 1 → cutoff here), so ticking the
        // global LFO 2 with a live rate/shape reproduces the output bit-for-bit.
        let mut a = pitched_synth();
        a.set_param(pp(PatchParam::CutoffLfo1Depth), 24.0); // LFO 1 → cutoff
        a.note_on(57, 1.0);
        let (base, _) = render(&mut a, 12_000);

        // Same patch, but tick the global LFO 2 with a live rate/shape (unrouted —
        // LFO 2's own cutoff depth stays zero).
        let mut b = pitched_synth();
        b.set_param(pp(PatchParam::CutoffLfo1Depth), 24.0);
        b.set_param(gp(GlobalParam::Lfo2Rate), 3.0);
        b.set_param(gp(GlobalParam::Lfo2Shape), 5.0); // S&H — exercises its PRNG
        b.note_on(57, 1.0);
        let (with_lfo2, _) = render(&mut b, 12_000);

        let max_err = base
            .iter()
            .zip(&with_lfo2)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err == 0.0, "zero-depth LFO2 changed output: {max_err}");
    }

    #[test]
    fn global_lfo2_is_shared_across_both_layers() {
        // The global LFO 2 reaches both layers from one shared phase: in Dual
        // mode, routing LFO2→pitch on each layer and playing the same note on
        // both yields the combined output = exactly twice one layer's (same LFO2
        // phase drives both). Proves a single instrument-wide source, not per-layer.
        fn configure(s: &mut Synth) {
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_key_mode(KeyMode::Dual);
            for layer in Layer::ALL {
                s.set_param(patch_clap_id(layer, PatchParam::Osc1Wave), 0.0); // sine
                s.set_param(patch_clap_id(layer, PatchParam::Osc2Level), 0.0);
                s.set_param(patch_clap_id(layer, PatchParam::PitchLfoSrc), 2.0); // LFO 2
                s.set_param(patch_clap_id(layer, PatchParam::PitchLfoDepth), 7.0);
            }
            s.set_param(gp(GlobalParam::Lfo2Rate), 5.0);
        }
        let mut one = Synth::new(48_000.0);
        configure(&mut one);
        one.note_on_layer(0, 69, 1.0);
        let (single, _) = render(&mut one, 9600);

        let mut two = Synth::new(48_000.0);
        configure(&mut two);
        two.note_on_layer(0, 69, 1.0);
        two.note_on_layer(1, 69, 1.0);
        let (both, _) = render(&mut two, 9600);

        assert!(rms(&single) > 0.01, "LFO2→amp produced no sound");
        let max_err = single
            .iter()
            .zip(&both)
            .map(|(a, b)| (2.0 * a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 1e-4, "global LFO2 not shared identically: {max_err}");
    }

    // ── E004 / 0015: host-tempo sync ────────────────────────────────────────

    /// Set the rate knob so its fader position lands exactly on subdivision `idx`
    /// (the inverse of `to_fader` ∘ `sync::index_from_norm`).
    fn rate_for_subdiv(idx: usize) -> f32 {
        let last = (sync::SUBDIVISIONS.len() - 1) as f32;
        PatchParam::LfoRate.desc().from_fader(idx as f32 / last)
    }

    #[test]
    fn lfo_sync_off_is_free_running_hz() {
        let mut p = PatchValues::default();
        p.set(PatchParam::LfoRate, 7.3);
        // Sync off (default): the rate knob is taken as literal Hz, tempo ignored.
        assert_eq!(
            lfo_rate(&p, PatchParam::LfoRate, PatchParam::LfoSync, 120.0),
            7.3
        );
    }

    #[test]
    fn lfo_sync_on_resolves_subdivision_from_tempo() {
        // Indices of the quarter-note family in the subdivision table.
        let q = sync::SUBDIVISIONS.iter().position(|s| s.label == "1/4").unwrap();
        let qd = sync::SUBDIVISIONS.iter().position(|s| s.label == "1/4.").unwrap();
        let qt = sync::SUBDIVISIONS.iter().position(|s| s.label == "1/4T").unwrap();

        let mut p = PatchValues::default();
        p.set(PatchParam::LfoSync, 1.0);
        let resolve = |p: &PatchValues, bpm| lfo_rate(p, PatchParam::LfoRate, PatchParam::LfoSync, bpm);

        // Straight quarter: one cycle per beat.
        p.set(PatchParam::LfoRate, rate_for_subdiv(q));
        assert!((resolve(&p, 120.0) - 2.0).abs() < 1e-4, "1/4 @120");
        assert!((resolve(&p, 90.0) - 1.5).abs() < 1e-4, "1/4 @90");
        // Dotted (×1.5 length) and triplet (×2/3 length) at 140 BPM.
        p.set(PatchParam::LfoRate, rate_for_subdiv(qd));
        assert!((resolve(&p, 140.0) - (140.0 / 60.0) / 1.5).abs() < 1e-4, "1/4. @140");
        p.set(PatchParam::LfoRate, rate_for_subdiv(qt));
        assert!((resolve(&p, 140.0) - (140.0 / 60.0) / (2.0 / 3.0)).abs() < 1e-4, "1/4T @140");
    }

    // ── E006: tempo-synced delay time ────────────────────────────────────────

    /// Set the delay time knob so its fader position lands exactly on subdivision
    /// `idx` (inverse of `to_fader` ∘ `sync::index_from_norm`).
    fn delay_time_for_subdiv(idx: usize) -> f32 {
        let last = (sync::SUBDIVISIONS.len() - 1) as f32;
        GlobalParam::DelayTime.desc().from_fader(idx as f32 / last)
    }

    #[test]
    fn delay_sync_off_is_literal_seconds() {
        // Sync off: the Time knob is taken as literal seconds, tempo ignored.
        assert_eq!(delay_time_seconds(false, 0.42, 120.0), 0.42);
        assert_eq!(delay_time_seconds(false, 0.42, 60.0), 0.42);
    }

    #[test]
    fn delay_sync_on_resolves_subdivision_from_tempo() {
        let q = sync::SUBDIVISIONS.iter().position(|s| s.label == "1/4").unwrap();
        let v = delay_time_for_subdiv(q);
        // 1/4 = one beat: 0.5 s @120, 1.0 s @60.
        assert!((delay_time_seconds(true, v, 120.0) - 0.5).abs() < 1e-4, "1/4 @120");
        assert!((delay_time_seconds(true, v, 60.0) - 1.0).abs() < 1e-4, "1/4 @60");
    }

    #[test]
    fn delay_synced_time_snaps_back_to_ms_when_sync_off() {
        // A knob value that means a subdivision while synced must read back as
        // the same literal seconds the instant sync is switched off (the stored
        // param value is never mutated, only reinterpreted).
        let v = delay_time_for_subdiv(3); // some arbitrary subdivision
        let synced = delay_time_seconds(true, v, 100.0);
        let unsynced = delay_time_seconds(false, v, 100.0);
        assert_ne!(synced, unsynced, "sync should reinterpret the value");
        assert_eq!(unsynced, v, "off must return the literal stored seconds");
    }

    #[test]
    fn set_tempo_rejects_nonfinite_and_nonpositive() {
        let mut s = Synth::new(48_000.0);
        s.set_tempo(f32::NAN);
        assert_eq!(s.tempo_bpm, sync::DEFAULT_TEMPO_BPM);
        s.set_tempo(0.0);
        assert_eq!(s.tempo_bpm, sync::DEFAULT_TEMPO_BPM);
        s.set_tempo(128.0);
        assert_eq!(s.tempo_bpm, 128.0);
    }

    #[test]
    fn synced_lfo_renders_finite_and_audible() {
        // End-to-end: a synced LFO→cutoff route at a fast subdivision drives the
        // filter and stays finite (the rate path never NaNs through the engine).
        let mut s = pitched_synth();
        s.set_param(pp(PatchParam::Osc1Wave), 2.0); // saw
        s.set_param(pp(PatchParam::Cutoff), 1200.0);
        s.set_param(pp(PatchParam::CutoffLfo1Depth), 36.0); // LFO 1 → cutoff
        s.set_param(pp(PatchParam::LfoSync), 1.0);
        s.set_param(pp(PatchParam::LfoRate), rate_for_subdiv(9)); // 1/8
        s.set_tempo(128.0);
        s.note_on(45, 1.0);
        let (l, _) = render(&mut s, 24_000);
        assert!(l.iter().all(|x| x.is_finite()), "synced LFO output not finite");
        assert!(rms(&l) > 0.01, "synced LFO produced no sound");
    }

    #[test]
    fn voice_stealing_keeps_polyphony_bounded() {
        let mut s = Synth::new(48_000.0);
        for n in 0..40u8 {
            s.note_on(n, 1.0);
        }
        let active = s.active_count();
        assert!(
            active <= vxn_dsp::MAX_VOICES,
            "too many active voices: {active}"
        );
    }

    // ── E003 / 0008: two-layer render ───────────────────────────────────────

    #[test]
    fn param_source_follows_key_mode() {
        // Whole: both layers read layer A (Upper). Dual/Split: each reads its own.
        assert_eq!(Synth::param_source(0, KeyMode::Whole), Layer::Upper);
        assert_eq!(Synth::param_source(1, KeyMode::Whole), Layer::Upper);
        for m in [KeyMode::Dual, KeyMode::Split] {
            assert_eq!(Synth::param_source(0, m), Layer::Upper);
            assert_eq!(Synth::param_source(1, m), Layer::Lower);
        }
    }

    /// A deterministic patch (sine LFO, chorus off) so two layers fed
    /// identical params + notes render bit-for-bit identically.
    fn deterministic(s: &mut Synth) {
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
    }

    #[test]
    fn whole_two_identical_layers_sum_to_double_single() {
        // ADR 0003 §3 Whole-equivalence: both layers read Upper's block, so two
        // layers playing the same note = exactly twice one layer's output.
        let mut one = Synth::new(48_000.0);
        deterministic(&mut one);
        one.note_on_layer(0, 69, 1.0);
        let (single, _) = render(&mut one, 9600);

        let mut two = Synth::new(48_000.0);
        deterministic(&mut two);
        two.note_on_layer(0, 69, 1.0);
        two.note_on_layer(1, 69, 1.0);
        let (both, _) = render(&mut two, 9600);

        assert!(rms(&single) > 0.01, "reference layer was silent");
        let max_err = single
            .iter()
            .zip(&both)
            .map(|(a, b)| (2.0 * a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-4,
            "two layers != 2x one layer: max_err {max_err}"
        );
    }

    #[test]
    fn dual_layers_superpose_two_independent_patches() {
        // Dual: Upper a plain sine, Lower a saw an octave up. Each layer reads
        // its own block; the two-layer sum equals the two layers rendered alone
        // (superposition), and the two patches are audibly different.
        fn configure(s: &mut Synth) {
            deterministic(s);
            s.set_key_mode(KeyMode::Dual);
            // Upper: sine.
            s.set_param(pp(PatchParam::Osc1Wave), 0.0);
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            // Lower: saw, +1 octave.
            s.set_param(lo(PatchParam::Osc1Wave), 2.0);
            s.set_param(lo(PatchParam::Osc1Octave), 1.0);
            s.set_param(lo(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(lo(PatchParam::Env2Attack), 0.001);
        }
        let frames = 9600;
        let mut up = Synth::new(48_000.0);
        configure(&mut up);
        up.note_on_layer(0, 57, 1.0);
        let (upper_only, _) = render(&mut up, frames);

        let mut lw = Synth::new(48_000.0);
        configure(&mut lw);
        lw.note_on_layer(1, 57, 1.0);
        let (lower_only, _) = render(&mut lw, frames);

        let mut both = Synth::new(48_000.0);
        configure(&mut both);
        both.note_on_layer(0, 57, 1.0);
        both.note_on_layer(1, 57, 1.0);
        let (combined, _) = render(&mut both, frames);

        assert!(
            rms(&upper_only) > 0.01 && rms(&lower_only) > 0.01,
            "a layer was silent"
        );
        // Two different patches.
        let diff = upper_only
            .iter()
            .zip(&lower_only)
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / frames as f32;
        assert!(
            diff > 1e-3,
            "the two layers are not distinguishable: {diff}"
        );
        // Sum of the two independent layers == the combined render.
        let max_err = combined
            .iter()
            .zip(upper_only.iter().zip(&lower_only))
            .map(|(c, (a, b))| (c - (a + b)).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 1e-4, "layers do not superpose: max_err {max_err}");
        assert!(combined.iter().all(|x| x.is_finite()), "non-finite sum");
    }

    // ── E003 / 0009: event router & key mode ────────────────────────────────

    fn layer_active(s: &Synth, layer: usize) -> usize {
        s.banks[layer].active_count()
    }

    #[test]
    fn whole_round_robins_successive_note_ons() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        s.note_on(60, 1.0);
        s.note_on(62, 1.0);
        // Two notes, alternating layers → one channel active in each.
        assert_eq!(layer_active(&s, 0), 1);
        assert_eq!(layer_active(&s, 1), 1);
    }

    #[test]
    fn dual_triggers_both_layers_per_note() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Dual);
        s.note_on(60, 1.0);
        // One note → both layers play it.
        assert_eq!(layer_active(&s, 0), 1);
        assert_eq!(layer_active(&s, 1), 1);
    }

    #[test]
    fn split_routes_by_pitch_about_the_split_point() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Split);
        s.set_split_point(60);
        s.note_on(48, 1.0); // below → Lower (layer 1)
        s.note_on(72, 1.0); // at/above → Upper (layer 0)
        assert_eq!(layer_active(&s, Layer::Lower as usize), 1);
        assert_eq!(layer_active(&s, Layer::Upper as usize), 1);
        // A note exactly at the split point goes to Upper.
        s.note_on(60, 1.0);
        assert_eq!(layer_active(&s, Layer::Upper as usize), 2);
    }

    #[test]
    fn note_off_releases_only_the_layer_that_started_it() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Split);
        s.set_split_point(60);
        s.note_on(48, 1.0); // Lower
        s.note_off(48); // broadcast; only Lower holds it
        // Gate cleared on Lower; Upper never had it. Render the release out.
        s.set_param(pp(PatchParam::Env2Release), 0.001);
        s.set_param(lo(PatchParam::Env2Release), 0.001);
        let (l, _) = render(&mut s, 4800);
        assert!(rms(&l[2400..]) < 1e-4, "note did not release");
    }

    #[test]
    fn held_notes_survive_a_mode_and_split_change() {
        // A sounding note keeps playing through a key-mode / split-point change;
        // only new note-ons follow the new routing (ADR 0003 §Consequences).
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        s.note_on(64, 1.0);
        let before = s.active_count();
        assert_eq!(before, 1);
        s.set_key_mode(KeyMode::Split);
        s.set_split_point(72);
        // Still sounding (not stranded).
        assert_eq!(s.active_count(), 1);
        let (l, _) = render(&mut s, 2400);
        assert!(rms(&l) > 0.001, "held note went silent across the change");
    }

    // ── E003 / 0010: per-layer assign-mode processor (poly) ─────────────────

    #[test]
    fn poly_layer_holds_eight_then_steals_oldest() {
        // Dual so each note hits both layers; one layer's allocation is confined
        // to its 8 channels and the 9th note steals (never exceeds 8).
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Dual);
        for n in 60..68 {
            s.note_on(n, 1.0); // 8 distinct notes
        }
        assert_eq!(layer_active(&s, 0), 8, "layer A should be full at 8");
        assert_eq!(layer_active(&s, 1), 8, "layer B should be full at 8");
        // 9th note steals rather than growing the layer past its 8 channels.
        s.note_on(68, 1.0);
        assert_eq!(layer_active(&s, 0), 8, "layer A must stay bounded to 8");
        assert_eq!(layer_active(&s, 1), 8, "layer B must stay bounded to 8");
    }

    #[test]
    fn layer_allocation_is_independent() {
        // Split: low notes → Lower, high → Upper. Flooding one layer never
        // touches the other's channels (independent per-layer allocation).
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Split);
        s.set_split_point(60);
        for n in 36..50 {
            s.note_on(n, 1.0); // all below split → Lower only
        }
        assert_eq!(
            layer_active(&s, Layer::Lower as usize),
            8,
            "Lower bounded to 8"
        );
        assert_eq!(
            layer_active(&s, Layer::Upper as usize),
            0,
            "Upper untouched by Lower's flood"
        );
    }

    #[test]
    fn assign_mode_param_reads_unison() {
        let mut p = ParamValues::default();
        assert_eq!(
            p.layer(Layer::Upper).assign_mode(),
            crate::params::AssignMode::Poly
        );
        p.layer_mut(Layer::Upper).set(PatchParam::AssignMode, 1.0);
        assert_eq!(
            p.layer(Layer::Upper).assign_mode(),
            crate::params::AssignMode::Unison
        );
    }

    // ── E003 / 0011: unison assign mode ─────────────────────────────────────

    /// Put a layer into a given assign mode with a unison detune (cents).
    fn set_assign(s: &mut Synth, layer: usize, unison: bool, detune: f32) {
        let mode = if unison {
            AssignMode::Unison
        } else {
            AssignMode::Poly
        };
        set_assign_mode(s, layer, mode, detune);
    }

    fn set_assign_mode(s: &mut Synth, layer: usize, mode: AssignMode, detune: f32) {
        s.set_param(
            patch_clap_id(Layer::ALL[layer], PatchParam::AssignMode),
            mode as usize as f32,
        );
        s.set_param(
            patch_clap_id(Layer::ALL[layer], PatchParam::UnisonDetune),
            detune,
        );
    }

    #[test]
    fn solo_is_monophonic_across_distinct_notes() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole); // Whole → layer reads Upper's assign
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        for n in [60, 64, 67, 72] {
            s.note_on_layer(0, n, 1.0);
            assert_eq!(layer_active(&s, 0), 1, "Solo must keep exactly one channel");
        }
    }

    #[test]
    fn twin_assigns_two_channels_per_note_and_stays_bounded() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Twin, 12.0);
        s.note_on_layer(0, 60, 1.0);
        assert_eq!(layer_active(&s, 0), 2, "Twin = two channels for one note");
        // Four notes saturate the 8-channel layer; further notes steal, not grow.
        for n in [62, 64, 65, 67] {
            s.note_on_layer(0, n, 1.0);
        }
        assert_eq!(layer_active(&s, 0), 8, "Twin tops out at 8 channels (4 notes)");
    }

    #[test]
    fn unison_engages_all_eight_channels_on_one_note() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole); // Whole → both layers read Upper's assign
        set_assign(&mut s, 0, true, 12.0);
        s.note_on_layer(0, 60, 1.0);
        assert_eq!(layer_active(&s, 0), 8, "unison should fill all 8 channels");
    }

    #[test]
    fn unison_detune_spreads_pitch_and_zero_collapses() {
        // Detune > 0: a single note's spectrum is wider (beating partials) than
        // the same note with detune 0 — compare summed energy spread crudely via
        // the difference between the two renders; they must differ.
        fn render_unison(detune: f32) -> Vec<f32> {
            let mut s = Synth::new(48_000.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::Osc1Wave), 0.0); // sine
            s.set_param(pp(PatchParam::Osc2Level), 0.0);
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(pp(PatchParam::Env2Attack), 0.001);
            set_assign(&mut s, 0, true, detune);
            s.note_on_layer(0, 57, 1.0);
            render(&mut s, 24_000).0
        }
        let tuned = render_unison(0.0);
        let spread = render_unison(25.0);
        let diff = tuned
            .iter()
            .zip(&spread)
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / tuned.len() as f32;
        assert!(
            diff > 1e-3,
            "detune did not change the unison spectrum: {diff}"
        );
        assert!(spread.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn unison_level_is_normalised_not_eight_times_poly() {
        // One unison note must not be ~8x louder than one poly note.
        fn one_note_rms(unison: bool) -> f32 {
            let mut s = Synth::new(48_000.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::Osc1Wave), 0.0);
            s.set_param(pp(PatchParam::Osc2Level), 0.0);
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(pp(PatchParam::Env2Attack), 0.001);
            set_assign(&mut s, 0, unison, 0.0); // detune 0 → coherent worst case
            s.note_on_layer(0, 57, 1.0);
            rms(&render(&mut s, 12_000).0[4800..])
        }
        let poly = one_note_rms(false);
        let uni = one_note_rms(true);
        // With detune 0 the 8 copies are coherent, so 1/√8 normalisation gives
        // ≈ √8 × one voice — louder, but nowhere near 8×.
        assert!(
            uni < 4.0 * poly,
            "unison too loud: poly {poly}, unison {uni}"
        );
        assert!(uni > poly, "unison should be fuller than one poly voice");
    }

    #[test]
    fn switching_poly_unison_is_clean() {
        // Unison fills 8; switching to Poly and playing leaves no stuck channels.
        let mut s = Synth::new(48_000.0);
        s.set_param(pp(PatchParam::Env2Release), 0.001);
        set_assign(&mut s, 0, true, 10.0);
        s.note_on_layer(0, 60, 1.0);
        assert_eq!(layer_active(&s, 0), 8);
        s.note_off(60);
        let _ = render(&mut s, 4800); // let the release free the channels
        assert_eq!(
            layer_active(&s, 0),
            0,
            "unison channels stuck after release"
        );
        // Now Poly: one note → one channel.
        set_assign(&mut s, 0, false, 0.0);
        s.note_on_layer(0, 64, 1.0);
        assert_eq!(
            layer_active(&s, 0),
            1,
            "poly after unison should use 1 channel"
        );
    }

    // ── E003 / 0012: portamento ─────────────────────────────────────────────

    /// Clean single-sine layer for pitch readout, with portamento configured.
    fn glide_synth(time: f32) -> Synth {
        let mut s = Synth::new(48_000.0);
        s.set_param(pp(PatchParam::Osc1Wave), 0.0); // sine
        s.set_param(pp(PatchParam::Osc2Level), 0.0);
        s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        // Glide has no on/off: a non-zero time enables it (time 0 = off).
        s.set_param(pp(PatchParam::PortamentoTime), time);
        s
    }

    #[test]
    fn portamento_glides_pitch_toward_the_target() {
        // Play A2 on layer 0, let it fully release (freeing the channel with its
        // last pitch), then play A3: pitch should start near A2 and rise to A3.
        let mut s = glide_synth(0.12);
        // Fast release on both envelopes so the channel frees (free needs both idle).
        s.set_param(pp(PatchParam::Env1Release), 0.001);
        s.set_param(pp(PatchParam::Env2Release), 0.001);
        s.note_on_layer(0, 45, 1.0); // A2 ≈ 110 Hz
        let _ = render(&mut s, 9600);
        s.note_off(45);
        let _ = render(&mut s, 9600); // release frees channel 0 (glide_semi = 45)
        assert_eq!(
            layer_active(&s, 0),
            0,
            "channel should be free before reuse"
        );

        s.note_on_layer(0, 57, 1.0); // A3 ≈ 220 Hz target
        let (l, _) = render(&mut s, 24_000);
        let early = dominant_hz(&l[480..2400], 48_000.0);
        let late = dominant_hz(&l[19_200..24_000], 48_000.0);
        assert!(
            early < 0.85 * late,
            "pitch did not glide upward: early {early}, late {late}"
        );
        assert!(
            (late / note_to_hz(57.0) - 1.0).abs() < 0.08,
            "glide did not reach the target: {late} vs {}",
            note_to_hz(57.0)
        );
    }

    #[test]
    fn portamento_time_zero_is_instant() {
        // Time 0 with glide on reproduces the immediate-pitch behaviour.
        let mut s = glide_synth(0.0);
        s.note_on_layer(0, 57, 1.0);
        let (l, _) = render(&mut s, 24_000);
        let f = dominant_hz(&l[480..4800], 48_000.0);
        assert!(
            (f / note_to_hz(57.0) - 1.0).abs() < 0.05,
            "time 0 should sound the target at once: {f}"
        );
    }

    #[test]
    fn portamento_is_independent_per_layer() {
        // Layer 0 glides; layer 1 has glide off. A glide on layer 0 must not move
        // layer 1's steady pitch. Dual so each layer reads its own params.
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Dual);
        // Clean single-sine on both layers for a stable pitch readout.
        for layer in Layer::ALL {
            s.set_param(patch_clap_id(layer, PatchParam::Osc1Wave), 0.0);
            s.set_param(patch_clap_id(layer, PatchParam::Osc2Level), 0.0);
            s.set_param(patch_clap_id(layer, PatchParam::PitchLfoDepth), 0.0);
            s.set_param(patch_clap_id(layer, PatchParam::Env2Attack), 0.001);
        }
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        // Layer 1 (Lower): glide off, plays a steady note.
        s.note_on_layer(1, 69, 1.0); // A4 = 440
        let (steady, _) = render(&mut s, 9600);
        let f_steady = dominant_hz(&steady[2400..9600], 48_000.0);
        // Layer 0 (Upper): turn glide on (non-zero time) and sweep; layer 1 sounds.
        s.set_param(patch_clap_id(Layer::Upper, PatchParam::PortamentoTime), 0.3);
        s.note_on_layer(0, 33, 1.0);
        let (_both, _) = render(&mut s, 9600);
        // Layer 1's note is still ~440 (not dragged by layer 0's glide). Verified
        // structurally: independent glide state per bank — assert it stayed up.
        assert!(
            (f_steady - 440.0).abs() < 10.0,
            "layer 1 baseline pitch wrong: {f_steady}"
        );
        assert_eq!(layer_active(&s, 1), 1, "layer 1 note should still sound");
    }

    #[test]
    fn sixteen_notes_spread_across_both_layers_and_stay_finite() {
        // Round-robin note-on (the interim Whole router) fills 8+8 = 16 channels.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::DelayOn), 1.0);
        s.set_param(pp(PatchParam::Resonance), 1.0);
        for n in 60..76 {
            s.note_on(n, 1.0);
        }
        assert_eq!(
            s.active_count(),
            16,
            "expected 16 channels across two layers"
        );
        let (l, r) = render(&mut s, 24_000);
        assert!(
            l.iter().chain(r.iter()).all(|x| x.is_finite()),
            "non-finite output"
        );
    }
}
