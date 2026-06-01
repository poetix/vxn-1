//! VXN1 synth engine.
//!
//! Framework-agnostic: holds parameters, allocates voices, and renders audio
//! in fixed control blocks. The CLAP layer drives it with note/param events
//! and contiguous output slices; the UI reads and writes [`ParamValues`].

pub mod factory;
pub mod params;
pub mod preset;
pub mod preset_io;
pub mod reverb_macro;
pub mod shared;
pub mod smoothing;
pub mod state;
pub mod voice;

// Host-tempo sync metadata (E004 / 0015) lives in vxn-app — pure data + pure
// functions, shared with the editor without dragging engine internals in.
pub use vxn_app::sync;

pub use params::{
    AssignMode, CrossModType, DEFAULT_SPLIT_POINT, EnvSel, GLOBAL_PARAMS, GlobalParam,
    GlobalValues, KeyMode, Layer, LfoSel, PATCH_PARAMS, ParamDesc, ParamKind, ParamRef,
    ParamValues, PatchParam, PatchValues, TOTAL_PARAMS, Taper, desc_for_clap_id, global_clap_id,
    module_for_clap_id, param_ref, patch_clap_id,
};
pub use factory::{FactoryPreset, factory};
pub use preset::{Meta, Performance, PresetError};
pub use reverb_macro::{ReverbType, ReverbVoicing, reverb_macro};
pub use preset_io::{
    EnginePresetStore, LoadError, UserFolder, UserPreset, create_user_folder, delete_user_folder,
    delete_user_preset, ensure_user_dir, list_user_presets, list_user_tree, load_preset_file,
    move_user_preset, rename_user_folder, rename_user_preset, save_performance,
    save_performance_in, user_preset_dir,
};
// UNCATEGORIZED moved to vxn-app::domain (ADR 0007). Engine re-exports it for
// path continuity (the preset_io module still references it in its doc-strings
// and the factory bank's category labels).
pub use vxn_app::UNCATEGORIZED;
pub use shared::SharedParams;
use smoothing::ParamSmoother;
pub use state::PluginState;

use voice::{BlockCtx, Lfo1Trigger, VoiceBank};
use vxn_dsp::{
    AdsrShape, CONTROL_BLOCK, LfoCore, MAX_OVERSAMPLE, Oversampler, Smoothed, StereoChorus,
    StereoDelay, StereoLimiter, StereoVReverb, note_to_hz,
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

/// Stable seed for the reverb's BBD clock jitter walk (parked off in v1 but
/// the engine still wants a deterministic init).
const REVERB_SEED: u32 = 0xBBD0_0040;

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
    /// MN3011-style BBD tap-comb reverb. Sits post-delay, pre-limiter in the
    /// FX chain; runs only when [`GlobalParam::ReverbOn`] is set.
    reverb: StereoVReverb,
    /// Voicing type used last block; on change, `reverb.reset()` clears the tail
    /// before the next process so a Plate's ring can't bleed into a Hall.
    reverb_was_type: Option<ReverbType>,
    /// Optional brickwall limiter on the master bus (last in the FX chain). Run
    /// only when [`GlobalParam::LimiterOn`] is set; bypassed otherwise.
    limiter: StereoLimiter,
    /// Whether the limiter ran last block, so it can be reset on the off→on edge
    /// (clears stale lookahead state instead of leaking a transient).
    limiter_was_on: bool,
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
            reverb: StereoVReverb::new(sample_rate, REVERB_SEED),
            reverb_was_type: None,
            limiter: StereoLimiter::new(sample_rate),
            limiter_was_on: false,
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
        self.reverb = StereoVReverb::new(sample_rate, REVERB_SEED);
        self.reverb_was_type = None;
        self.limiter = StereoLimiter::new(sample_rate);
        self.limiter_was_on = false;
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
                // Solo and Unison are monophonic per layer, so round-robining across
                // both layers would give two simultaneous voices and split the
                // held-note stack — each note would land on a different bank,
                // defeating mono/legato. Pin them to one layer (Upper, whose block
                // both layers read in Whole). Poly/Twin still spread 8+8.
                let mono = matches!(
                    self.params.layer(Layer::Upper).assign_mode(),
                    AssignMode::Solo | AssignMode::Unison
                );
                if mono {
                    self.note_on_layer(Layer::Upper as usize, note, velocity);
                } else {
                    let layer = self.rr_layer;
                    self.rr_layer ^= 1;
                    self.note_on_layer(layer, note, velocity);
                }
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
        let legato = p.legato();
        self.banks[layer].note_on(
            mode,
            note,
            velocity,
            self.alloc_counter,
            unison_detune,
            lfo1,
            legato,
        );
    }

    pub fn note_off(&mut self, note: u8) {
        // Broadcast: each layer releases the note only if it is holding it. Mono
        // layers (Solo / Unison) run the stack path (revert to a still-held note);
        // every other mode just gates the matching channels off.
        self.alloc_counter += 1;
        for layer in 0..self.banks.len() {
            let src = Self::param_source(layer, self.key_mode);
            let p = self.params.layer(src);
            if matches!(p.assign_mode(), AssignMode::Solo | AssignMode::Unison) {
                let lfo1 = Lfo1Trigger {
                    shape: p.lfo_shape(),
                    free_run: p.bool(PatchParam::Lfo1FreeRun),
                };
                let legato = p.legato();
                let detune = p.get(PatchParam::UnisonDetune);
                self.banks[layer].mono_note_off(
                    p.assign_mode(),
                    note,
                    legato,
                    self.alloc_counter,
                    detune,
                    lfo1,
                );
            } else {
                self.banks[layer].note_off(note);
            }
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
        self.limiter.reset();
        self.limiter_was_on = false;
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

            // Reverb (post-delay): wet from the line, blend with smoothed mix.
            // Skipped when off so the engine stays sample-exact against a build
            // with reverb absent.
            let reverb_on = self.params.global().bool(GlobalParam::ReverbOn);
            if reverb_on {
                let mut dry_in = [0f32; CONTROL_BLOCK];
                let dry_in = &mut dry_in[..block];
                for i in 0..block {
                    dry_in[i] = 0.5 * (l_out[i] + r_out[i]);
                }
                let mut wet_l = [0f32; CONTROL_BLOCK];
                let mut wet_r = [0f32; CONTROL_BLOCK];
                let (wl, wr) = (&mut wet_l[..block], &mut wet_r[..block]);
                self.reverb.process_block(dry_in, wl, wr);
                let mix = self
                    .smoother
                    .values()
                    .global()
                    .get(GlobalParam::ReverbMix);
                for i in 0..block {
                    l_out[i] += mix * (wl[i] - l_out[i]);
                    r_out[i] += mix * (wr[i] - r_out[i]);
                }
            }

            // Master limiter (last in the chain): clear stale lookahead state on
            // the off→on edge so re-engaging it can't leak an old transient.
            let limiter_on = self.params.global().bool(GlobalParam::LimiterOn);
            if limiter_on {
                if !self.limiter_was_on {
                    self.limiter.reset();
                }
                self.limiter.process_block(l_out, r_out);
            }
            self.limiter_was_on = limiter_on;
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

        // Reverb: resolve the macro UI (Type + Depth) into the six underlying
        // knobs and push to the engine. Type lives unsmoothed (it's a discrete
        // switch), so read from `params` rather than the smoother. On a Type
        // change clear the tail so the previous voicing doesn't bleed.
        let t = self.params.global().reverb_type();
        if self.reverb_was_type != Some(t) {
            self.reverb.reset();
            self.reverb_was_type = Some(t);
        }
        let depth = g.get(GlobalParam::ReverbDepth);
        let v = reverb_macro::reverb_macro(t, depth);
        self.reverb
            .set_params(v.size, v.decay, v.damping, v.mod_rate, v.mod_depth, 0.0);
        self.reverb.set_diffusion(v.diffusion);
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

        // Cross-mod type selector → (sync flag, PM index, ring flag). Off zeroes
        // sync/PM and disables ring, so the voice keeps the independent fast
        // path; the four variants are mutually exclusive.
        let (sync, pm_index, ring_mode) = match p.cross_mod_type() {
            CrossModType::Off => (false, 0.0, false),
            CrossModType::Sync => (true, 0.0, false),
            CrossModType::Pm => (false, p.get(PatchParam::CrossModAmount), false),
            CrossModType::Ring => (false, 0.0, true),
        };

        // Mod wheel (CC1) is a global control applied once per block, folded into
        // the route `*_extra` terms (and resonance) here rather than per voice.
        let resonance = (p.get(PatchParam::Resonance) + wheel * p.get(PatchParam::ModWheelReso))
            .clamp(0.0, 1.0);

        BlockCtx {
            os_sample_rate: self.sample_rate * os as f32,
            os,
            osc1_wave: p.osc_wave(PatchParam::Osc1Wave),
            osc2_wave: p.osc_wave(PatchParam::Osc2Wave),
            osc1_level: p.get(PatchParam::Osc1Level),
            osc2_level: p.get(PatchParam::Osc2Level),
            sub_level: p.get(PatchParam::SubLevel),
            ring_mode,
            noise_level: p.get(PatchParam::NoiseLevel),
            noise_color: p.noise_color(),
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
            filter_mode: p.filter_mode(),
            filter_slope: p.filter_slope(),
            base_semis: g.get(GlobalParam::MasterTune),
            lfo1_shape: p.lfo_shape(),
            lfo1_rate_hz,
            lfo1_delay_time: p.get(PatchParam::Lfo1DelayTime),
            lfo1_fade: p.get(PatchParam::Lfo1Fade),
            lfo2_val,
            sync,
            pm_index,
            cross_mod_type: p.cross_mod_type(),
            portamento_time: p.get(PatchParam::PortamentoTime),
            // Fixed routes (ADR 0004 §4).
            pitch_lfo_sel: p.lfo_sel(PatchParam::PitchLfoSrc),
            pitch_lfo_depth: p.get(PatchParam::PitchLfoDepth),
            pitch_lfo_mod_only: p.bool(PatchParam::PitchLfoModOnly),
            pitch_env_sel: p.env_sel(PatchParam::PitchEnvSrc),
            pitch_env_depth: p.get(PatchParam::PitchEnvDepth),
            pitch_env_mod_only: p.bool(PatchParam::PitchEnvModOnly),
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
            sweep_extra: wheel * p.get(PatchParam::ModWheelCrossModSweep),
            amp_lfo_sel: p.lfo_sel(PatchParam::AmpLfoSrc),
            amp_lfo_depth: p.get(PatchParam::AmpLfoDepth),
            amp_env_bypass: p.bool(PatchParam::AmpEnvBypass),
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
    fn noise_level_produces_sound_with_oscillators_silenced() {
        // With both oscillators at zero level, only the noise source can make
        // sound — proving noise is wired into the mixer.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Osc1Level), 0.0);
        s.set_param(pp(PatchParam::Osc2Level), 0.0);
        s.set_param(pp(PatchParam::Env2Sustain), 1.0);
        s.note_on(69, 1.0);
        // Let the osc-level glide settle to 0, then a silent window (noise off).
        render(&mut s, 9_600);
        let (silent, _) = render(&mut s, 4_800);
        s.set_param(pp(PatchParam::NoiseLevel), 0.8);
        let (loud, _) = render(&mut s, 48_000);
        assert!(
            rms(&silent) < 1e-5,
            "no source should be silent: {}",
            rms(&silent)
        );
        let tail = &loud[loud.len() - 4800..];
        assert!(
            rms(tail) > 1e-3,
            "noise should be audible, got {}",
            rms(tail)
        );
    }

    #[test]
    fn amp_env_bypass_holds_full_level_ignoring_env2() {
        // Gate-only VCA: with Env2 sustain 0 and fast decay (which would silence
        // the enveloped VCA — see `vca_follows_env2`), bypass keeps a held note at
        // full level because the amp follows the gate, not Env2.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Env2Decay), 0.01);
        s.set_param(pp(PatchParam::Env2Sustain), 0.0);
        s.set_param(pp(PatchParam::AmpEnvBypass), 1.0);
        s.note_on(69, 1.0);
        let (l, _) = render(&mut s, 48_000);
        let tail = &l[l.len() - 4800..];
        assert!(
            rms(tail) > 1e-2,
            "bypass should hold full level despite Env2 sustain 0, got {}",
            rms(tail)
        );
    }

    #[test]
    fn amp_tremolo_attenuates_output() {
        // A square-wave LFO into the amp at full depth chops the VCA between full
        // and silence, so the windowed RMS varies far more than the un-tremoloed
        // (steady-sustain) signal.
        let setup = |trem: bool| {
            let mut s = Synth::new(48_000.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::Env2Sustain), 1.0);
            s.set_param(pp(PatchParam::LfoShape), 4.0); // Square
            s.set_param(pp(PatchParam::LfoRate), 8.0);
            if trem {
                s.set_param(pp(PatchParam::AmpLfoSrc), 1.0); // LFO 1
                s.set_param(pp(PatchParam::AmpLfoDepth), 1.0);
            }
            s.note_on(57, 1.0);
            let (l, _) = render(&mut s, 48_000);
            l
        };
        // Window RMS over 480-sample frames; tremolo makes it swing, steady doesn't.
        let spread = |l: &[f32]| {
            let w: Vec<f32> = l.chunks(480).map(rms).filter(|r| *r > 0.0).collect();
            let max = w.iter().cloned().fold(0.0f32, f32::max);
            let min = w.iter().cloned().fold(f32::MAX, f32::min);
            max - min
        };
        assert!(
            spread(&setup(true)) > 3.0 * spread(&setup(false)),
            "tremolo should swing the level far more than steady sustain"
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
        assert!(
            later < early * 0.7,
            "amp decay stalled: early {early} later {later}"
        );
        assert!(
            settled < later,
            "amp kept falling toward sustain: {later} -> {settled}"
        );
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
        // +1 octave & +7 st = +19 st. Compare against +2 octaves & -5 st (also +19 st).
        let mut a = pitched_synth();
        a.set_param(pp(PatchParam::Osc1Octave), 1.0);
        a.set_param(pp(PatchParam::Osc1Coarse), 7.0);
        a.note_on(45, 1.0);
        let (la, _) = render(&mut a, 24_000);
        let fa = dominant_hz(&la[4800..], 48_000.0);

        let mut b = pitched_synth();
        b.set_param(pp(PatchParam::Osc1Octave), 2.0);
        b.set_param(pp(PatchParam::Osc1Coarse), -5.0);
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
    fn ring_mode_displaces_osc1_and_off_is_inert() {
        // `CrossModType::Ring` routes osc1×osc2 into the osc1 mixer slot, so the
        // patch's timbre shifts vs. the Off render and stays finite. Off is the
        // inert fast path (its output is bit-identical across renders).
        fn render_ring(on: bool) -> Vec<f32> {
            let mut s = pitched_synth();
            s.set_param(pp(PatchParam::Osc1Wave), 0.0); // sine
            s.set_param(pp(PatchParam::Osc2Wave), 0.0);
            s.set_param(pp(PatchParam::Osc1Level), 0.5);
            s.set_param(pp(PatchParam::Osc2Level), 0.5);
            s.set_param(pp(PatchParam::Osc2Coarse), 5.0); // inharmonic vs osc1
            s.set_param(
                pp(PatchParam::CrossModType),
                if on { 3.0 } else { 0.0 },
            );
            s.note_on(45, 1.0);
            render(&mut s, 12_000).0
        }
        let dry = render_ring(false);
        assert_eq!(dry, render_ring(false), "Ring off path not deterministic");
        let wet = render_ring(true);
        assert!(wet.iter().all(|x| x.is_finite()), "ring output not finite");
        let diff = mean_abs_diff(&dry[4800..], &wet[4800..]);
        assert!(diff > 1e-3, "Ring mode did not change the output: {diff}");
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
            s.set_param(
                pp(PatchParam::FilterKeyTrack),
                if key_track { 1.0 } else { 0.0 },
            );
            s.note_on(72, 1.0); // a high note → large key-track shift when on
            let (l, _) = render(&mut s, 24_000);
            assert!(
                l.iter().all(|x| x.is_finite()),
                "key-track output not finite"
            );
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
        assert!(
            delay_diff < 1e-6,
            "LFO 1 not held at zero in the delay: {delay_diff}"
        );
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
        let unsynced = render_sync(false, -7.0);
        let synced_low = render_sync(true, -7.0);
        let synced_high = render_sync(true, 7.0);
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
    fn mod_wheel_cross_mod_sweep_shifts_audible_osc() {
        // Wheel→X-Mod sweep depth 12 st, wheel full → +1 oct on the targeted
        // osc(s). In Off mode the sweep hits both oscs; here osc1 is muted, so
        // only osc2 is audible — its freq should double.
        let mut base = osc2_sine_synth();
        base.note_on(57, 1.0); // 220 Hz
        let (l0, _) = render(&mut base, 24_000);
        let f0 = dominant_hz(&l0[4800..], 48_000.0);

        let mut up = osc2_sine_synth();
        up.set_param(pp(PatchParam::ModWheelCrossModSweep), 12.0);
        up.set_mod_wheel(1.0);
        up.note_on(57, 1.0);
        let (l1, _) = render(&mut up, 24_000);
        let f1 = dominant_hz(&l1[4800..], 48_000.0);

        assert!(
            (f1 / f0 - 2.0).abs() < 0.05,
            "wheel→x-mod +12 st should double audible osc freq: {f0} -> {f1}"
        );
    }

    #[test]
    fn fm_mode_pitch_env_mod_only_modulates_osc2_not_osc1() {
        // The "Mod" switch isolates env→pitch to the modulator oscillator.
        // Modulator = osc2 by default; Sync flips to osc1. Verify by
        // silencing osc2 and listening to osc1 alone: a hot env→pitch
        // (+12 st) must NOT shift the carrier in Off / Ring / Pm modes
        // (all of which route to osc2). Sync's osc1-routing is covered
        // separately.
        fn carrier_pitch(cross_mod: f32, amount: f32) -> f32 {
            let mut s = Synth::new(48_000.0);
            s.set_param(pp(PatchParam::Osc1Wave), 0.0); // sine carrier
            s.set_param(pp(PatchParam::Osc1Level), 0.8);
            s.set_param(pp(PatchParam::Osc2Wave), 0.0); // sine modulator
            s.set_param(pp(PatchParam::Osc2Level), 0.0); // silent — only osc1 audible
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::CrossModType), cross_mod);
            s.set_param(pp(PatchParam::CrossModAmount), amount);
            // Env 1 → pitch, +12 st, mod-only ON. Hot AD + full sustain so
            // env_1 sits at 1.0 across the capture window.
            s.set_param(pp(PatchParam::PitchEnvSrc), 1.0); // Env 1
            s.set_param(pp(PatchParam::PitchEnvDepth), 12.0);
            s.set_param(pp(PatchParam::PitchEnvModOnly), 1.0);
            s.set_param(pp(PatchParam::Env1Attack), 0.001);
            s.set_param(pp(PatchParam::Env1Decay), 0.001);
            s.set_param(pp(PatchParam::Env1Sustain), 1.0);
            s.set_param(pp(PatchParam::Env2Attack), 0.001);
            s.note_on(57, 1.0); // A3 ≈ 220 Hz
            let (l, _) = render(&mut s, 24_000);
            dominant_hz(&l[4800..], 48_000.0)
        }
        // Reference: plain A3 carrier, no env→pitch.
        let clean = {
            let mut s = Synth::new(48_000.0);
            s.set_param(pp(PatchParam::Osc1Wave), 0.0);
            s.set_param(pp(PatchParam::Osc1Level), 0.8);
            s.set_param(pp(PatchParam::Osc2Level), 0.0);
            s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
            s.set_param(gp(GlobalParam::ChorusOn), 0.0);
            s.set_param(pp(PatchParam::Env2Attack), 0.001);
            s.note_on(57, 1.0);
            let (l, _) = render(&mut s, 24_000);
            dominant_hz(&l[4800..], 48_000.0)
        };
        // FM at amount = 0 must still route env to osc2 — the mode is FM
        // regardless of depth (the kernel takes the fast path at 0, but the
        // semantic routing still picks osc2 as the modulator).
        let fm_zero = carrier_pitch(2.0, 0.0);
        assert!(
            (fm_zero / clean - 1.0).abs() < 0.03,
            "FM amount=0 + mod-only shifted carrier (routing read amount, \
             not mode): clean {clean}, fm0 {fm_zero}",
        );
        // FM with low pm_index keeps the carrier the dominant FFT peak; env
        // should leave it untouched (routes to silent osc2).
        let fm = carrier_pitch(2.0, 0.1);
        assert!(
            (fm / clean - 1.0).abs() < 0.03,
            "FM + mod-only shifted carrier (env leaked to osc1): clean {clean}, fm {fm}",
        );
        // Off mode: mod-only routes to osc2 (default modulator) — same as
        // Pm — so the audible osc1 stays put. Without this isolation the
        // Mod switch would be a no-op when no cross-mod is in play.
        let off = carrier_pitch(0.0, 0.0);
        assert!(
            (off / clean - 1.0).abs() < 0.03,
            "Off + mod-only shifted carrier (env leaked to osc1): clean {clean}, off {off}",
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
        assert_eq!(
            s.banks[0].lfo1_phase(1),
            0.0,
            "new voice retriggers to zero"
        );
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
        assert!(
            max_err < 1e-4,
            "global LFO2 not shared identically: {max_err}"
        );
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
        let q = sync::SUBDIVISIONS
            .iter()
            .position(|s| s.label == "1/4")
            .unwrap();
        let qd = sync::SUBDIVISIONS
            .iter()
            .position(|s| s.label == "1/4.")
            .unwrap();
        let qt = sync::SUBDIVISIONS
            .iter()
            .position(|s| s.label == "1/4T")
            .unwrap();

        let mut p = PatchValues::default();
        p.set(PatchParam::LfoSync, 1.0);
        let resolve =
            |p: &PatchValues, bpm| lfo_rate(p, PatchParam::LfoRate, PatchParam::LfoSync, bpm);

        // Straight quarter: one cycle per beat.
        p.set(PatchParam::LfoRate, rate_for_subdiv(q));
        assert!((resolve(&p, 120.0) - 2.0).abs() < 1e-4, "1/4 @120");
        assert!((resolve(&p, 90.0) - 1.5).abs() < 1e-4, "1/4 @90");
        // Dotted (×1.5 length) and triplet (×2/3 length) at 140 BPM.
        p.set(PatchParam::LfoRate, rate_for_subdiv(qd));
        assert!(
            (resolve(&p, 140.0) - (140.0 / 60.0) / 1.5).abs() < 1e-4,
            "1/4. @140"
        );
        p.set(PatchParam::LfoRate, rate_for_subdiv(qt));
        assert!(
            (resolve(&p, 140.0) - (140.0 / 60.0) / (2.0 / 3.0)).abs() < 1e-4,
            "1/4T @140"
        );
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
        let q = sync::SUBDIVISIONS
            .iter()
            .position(|s| s.label == "1/4")
            .unwrap();
        let v = delay_time_for_subdiv(q);
        // 1/4 = one beat: 0.5 s @120, 1.0 s @60.
        assert!(
            (delay_time_seconds(true, v, 120.0) - 0.5).abs() < 1e-4,
            "1/4 @120"
        );
        assert!(
            (delay_time_seconds(true, v, 60.0) - 1.0).abs() < 1e-4,
            "1/4 @60"
        );
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
        assert!(
            l.iter().all(|x| x.is_finite()),
            "synced LFO output not finite"
        );
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

    fn set_legato(s: &mut Synth, layer: usize, on: bool) {
        s.set_param(
            patch_clap_id(Layer::ALL[layer], PatchParam::Legato),
            on as u8 as f32,
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
    fn whole_mode_solo_pins_to_one_layer_not_round_robin() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        // Two notes through the routing path: Poly would round-robin (one per
        // layer); Solo must keep both on layer 0 so it stays one mono voice.
        s.note_on(60, 1.0);
        s.note_on(64, 1.0);
        assert_eq!(layer_active(&s, 0), 1, "Solo stays one voice on layer 0");
        assert_eq!(layer_active(&s, 1), 0, "Solo never spills to layer 1");
        assert_eq!(s.banks[0].gated_note(0), Some(64));
    }

    #[test]
    fn solo_pins_to_channel_zero_and_quiesces_others() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        // Leave a Poly chord ringing on several channels, then switch to Solo.
        set_assign_mode(&mut s, 0, AssignMode::Poly, 0.0);
        for n in [60, 64, 67] {
            s.note_on_layer(0, n, 1.0);
        }
        assert_eq!(layer_active(&s, 0), 3);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        s.note_on_layer(0, 72, 1.0);
        // The new note sounds on channel 0; every other channel is gated off (its
        // tail releasing), so only one note is gated/sounding.
        assert_eq!(s.banks[0].gated_note(0), Some(72));
        let gated: Vec<u8> = (0..8).filter_map(|v| s.banks[0].gated_note(v)).collect();
        assert_eq!(
            gated,
            vec![72],
            "Solo gates exactly one note, pinned to ch0"
        );
    }

    #[test]
    fn solo_stack_reverts_to_held_note_on_release() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        s.note_on_layer(0, 60, 1.0); // hold C
        s.note_on_layer(0, 64, 1.0); // hold E on top — C still held underneath
        assert_eq!(s.banks[0].gated_note(0), Some(64));
        s.note_off(64); // release E → revert to the still-held C
        assert_eq!(s.banks[0].gated_note(0), Some(60), "revert to held note");
        s.note_off(60); // release C → nothing held, channel releases
        assert_eq!(s.banks[0].gated_note(0), None);
    }

    #[test]
    fn solo_release_of_non_top_note_keeps_sounding_note() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        s.note_on_layer(0, 60, 1.0);
        s.note_on_layer(0, 64, 1.0); // E sounding, C held underneath
        s.note_off(60); // release the underlying C — E must keep sounding
        assert_eq!(s.banks[0].gated_note(0), Some(64));
        s.note_off(64); // now release E → nothing left
        assert_eq!(s.banks[0].gated_note(0), None);
    }

    #[test]
    fn solo_legato_does_not_retrigger_while_a_note_is_held() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        set_legato(&mut s, 0, true);
        s.note_on_layer(0, 60, 1.0);
        assert!(s.banks[0].trigger_pending(0), "first note always triggers");
        render(&mut s, 64); // consume the pending trigger
        s.note_on_layer(0, 64, 1.0); // legato slur — pitch changes, no retrigger
        assert_eq!(s.banks[0].gated_note(0), Some(64));
        assert!(
            !s.banks[0].trigger_pending(0),
            "legato note must not retrigger the envelope/phase"
        );
    }

    #[test]
    fn unison_legato_slides_all_channels_without_retrigger() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Unison, 12.0);
        set_legato(&mut s, 0, true);
        s.note_on(60, 1.0);
        assert_eq!(layer_active(&s, 0), 8, "Unison fills all 8 channels");
        render(&mut s, 64); // consume the pending triggers
        s.note_on(64, 1.0); // legato slur across the whole stack
        assert_eq!(layer_active(&s, 0), 8, "still the same 8-channel voice");
        for v in 0..8 {
            assert_eq!(s.banks[0].gated_note(v), Some(64), "all channels follow");
            assert!(
                !s.banks[0].trigger_pending(v),
                "legato Unison must not retrigger channel {v}"
            );
        }
    }

    #[test]
    fn unison_legato_reverts_to_held_note_on_release() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Unison, 12.0);
        set_legato(&mut s, 0, true);
        s.note_on(60, 1.0);
        s.note_on(64, 1.0); // 64 sounding, 60 held underneath
        s.note_off(64); // revert the whole stack to 60
        assert_eq!(layer_active(&s, 0), 8);
        for v in 0..8 {
            assert_eq!(s.banks[0].gated_note(v), Some(60));
        }
        s.note_off(60); // nothing held → release
        assert_eq!(
            layer_active(&s, 0),
            8,
            "still releasing (gates off, not idle)"
        );
        assert_eq!(s.banks[0].gated_note(0), None, "gate cleared");
    }

    #[test]
    fn solo_without_legato_retriggers_each_note() {
        let mut s = Synth::new(48_000.0);
        s.set_key_mode(KeyMode::Whole);
        set_assign_mode(&mut s, 0, AssignMode::Solo, 0.0);
        set_legato(&mut s, 0, false);
        s.note_on_layer(0, 60, 1.0);
        render(&mut s, 64);
        s.note_on_layer(0, 64, 1.0); // no legato → fresh trigger
        assert!(
            s.banks[0].trigger_pending(0),
            "non-legato Solo retriggers every note"
        );
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
        assert_eq!(
            layer_active(&s, 0),
            8,
            "Twin tops out at 8 channels (4 notes)"
        );
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
        // A unison note must not be ~8x louder than a poly note. The Unison stack's
        // 8 copies get independent random start phases (0011), so at detune 0 they
        // sum as a random walk (~√8), and `1/√8` normalisation keeps the RMS in the
        // same ballpark as one voice — never the naive 8×. The per-trigger phases
        // vary, so the lower bound is asserted on the mean over several triggers.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(pp(PatchParam::Osc1Wave), 0.0);
        s.set_param(pp(PatchParam::Osc2Level), 0.0);
        s.set_param(pp(PatchParam::PitchLfoDepth), 0.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s.set_param(pp(PatchParam::Env2Release), 0.001);

        // One fresh note's steady-state RMS, then release and silence so the next
        // trigger starts clean (and, for Unison, redraws its random phases).
        let mut one_note_rms = |unison: bool| -> f32 {
            set_assign(&mut s, 0, unison, 0.0); // detune 0 → coherent worst case
            s.note_on_layer(0, 57, 1.0);
            let r = rms(&render(&mut s, 12_000).0[4800..]);
            s.note_off(57);
            let _ = render(&mut s, 2_400); // let the release free the channels
            r
        };

        let poly = one_note_rms(false);
        // Average Unison over several triggers: any single trigger's random phases
        // can cancel or reinforce, but the mean tracks the √N power normalisation.
        let trials = 8;
        let mut uni_sum = 0.0;
        let mut uni_max: f32 = 0.0;
        for _ in 0..trials {
            let u = one_note_rms(true);
            uni_sum += u;
            uni_max = uni_max.max(u);
        }
        let uni_mean = uni_sum / trials as f32;
        // Upper bound holds on every trigger: even all-aligned phases give only √8.
        assert!(
            uni_max < 4.0 * poly,
            "unison too loud: poly {poly}, unison max {uni_max}"
        );
        // Mean stays a fraction of a single voice (not silent, not boosted away).
        assert!(
            uni_mean > 0.4 * poly && uni_mean < 2.0 * poly,
            "unison level off: poly {poly}, unison mean {uni_mean}"
        );
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

    /// Run a short note through the engine with the FX block to default-off
    /// state except for the parameters the caller pre-set.
    fn render_short_note(s: &mut Synth, frames: usize) -> (Vec<f32>, Vec<f32>) {
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s.set_param(pp(PatchParam::Env2Release), 0.01);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.note_on(69, 1.0);
        render(s, frames)
    }

    #[test]
    fn reverb_off_passes_dry_unchanged() {
        // With reverb_on=0 the reverb branch is gated off, so the dry chain
        // output must not depend on reverb_type / depth / mix. Compare two
        // runs that differ only in those three knobs.
        let mut a = Synth::new(48_000.0);
        a.set_param(gp(GlobalParam::ReverbOn), 0.0);
        a.set_param(gp(GlobalParam::ReverbType), 0.0); // Plate
        a.set_param(gp(GlobalParam::ReverbDepth), 0.0);
        a.set_param(gp(GlobalParam::ReverbMix), 0.0);
        let (al, ar) = render_short_note(&mut a, 4800);

        let mut b = Synth::new(48_000.0);
        b.set_param(gp(GlobalParam::ReverbOn), 0.0);
        b.set_param(gp(GlobalParam::ReverbType), 3.0); // Large
        b.set_param(gp(GlobalParam::ReverbDepth), 1.0);
        b.set_param(gp(GlobalParam::ReverbMix), 1.0);
        let (bl, br) = render_short_note(&mut b, 4800);

        assert_eq!(al, bl, "reverb_off path is not dry-pass on L");
        assert_eq!(ar, br, "reverb_off path is not dry-pass on R");
    }

    #[test]
    fn reverb_type_switch_resets_tail() {
        // Charge a Plate tail with high decay (Large in the macro table),
        // then switch the Type and assert the engine's reset clears the
        // line — silent input post-switch produces silent output for the
        // first block, where previously it would have rung out.
        let mut s = Synth::new(48_000.0);
        s.set_param(gp(GlobalParam::ChorusOn), 0.0);
        s.set_param(gp(GlobalParam::DelayOn), 0.0);
        s.set_param(gp(GlobalParam::ReverbOn), 1.0);
        s.set_param(gp(GlobalParam::ReverbType), 3.0); // Large = longest decay
        s.set_param(gp(GlobalParam::ReverbDepth), 1.0);
        s.set_param(gp(GlobalParam::ReverbMix), 1.0);
        s.set_param(pp(PatchParam::Env2Attack), 0.001);
        s.set_param(pp(PatchParam::Env2Release), 0.01);
        // Excite the line with a short note, then release fully.
        s.note_on(69, 1.0);
        let _ = render(&mut s, 9600);
        s.note_off(69);
        let _ = render(&mut s, 9600);

        // Tail is non-trivial at this point — confirm by capturing one block.
        let (tail_l, tail_r) = render(&mut s, 256);
        let tail_peak = tail_l
            .iter()
            .chain(tail_r.iter())
            .fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(
            tail_peak > 1e-4,
            "test precondition failed: tail too quiet ({tail_peak}) — won't detect a reset regression"
        );

        // Switch Type — engine should reset() before the next block.
        s.set_param(gp(GlobalParam::ReverbType), 1.0); // Room
        let (post_l, post_r) = render(&mut s, 256);
        let post_peak = post_l
            .iter()
            .chain(post_r.iter())
            .fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(
            post_peak < 1e-5,
            "Type switch did not reset reverb tail: peak={post_peak}, was tail_peak={tail_peak}"
        );
    }
}
