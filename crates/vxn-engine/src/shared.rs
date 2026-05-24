//! Thread-safe parameter store shared between the audio thread, the main thread
//! and the UI.
//!
//! Indexed by **CLAP id** (the host/UI boundary speaks ids — see
//! [`crate::params`] for the layout): a flat `[AtomicU32; TOTAL_PARAMS]` of plain
//! `f32` values stored as bits. This is the single source of truth that all
//! writers (host automation, UI edits, state load) update and all readers
//! observe:
//!
//! - **Host → engine/UI:** the CLAP layer applies input param events to this
//!   store; the audio thread snapshots it into the engine each block; the UI
//!   polls it on repaint.
//! - **UI → host:** the UI writes the new value here and raises a *gesture*
//!   ([`set_gesture`]) while a knob is held; the CLAP layer diffs this store
//!   against a per-thread mirror and emits the change (wrapped in gesture
//!   begin/end) to the host as output param events.
//!
//! Alongside the param array it carries the **non-automatable shared state**
//! (key mode + split point — ADR 0003 §3, §8) as atomics: setup state, not
//! sound parameters, set discretely from the UI and persisted via
//! [`crate::state`].
//!
//! Kept in `vxn-engine` (framework-free) so both `vxn-clap` and `vxn-ui` share
//! one definition without depending on each other.

use crate::params::{
    KeyMode, Layer, ParamValues, PatchParam, TOTAL_PARAMS, desc_for_clap_id, patch_clap_id,
};
use crate::state::PluginState;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

/// Atomic, lock-free parameter store. Intended to live behind an `Arc` so the
/// editor and the plugin can both hold it.
pub struct SharedParams {
    values: [AtomicU32; TOTAL_PARAMS],
    /// Whether the UI is currently holding an edit gesture (e.g. pointer down)
    /// on each param. Read by the plugin to bracket output events in CLAP
    /// gesture begin/end.
    gesture: [AtomicBool; TOTAL_PARAMS],
    /// Key mode (ADR 0003 §3) — non-automatable shared state.
    key_mode: AtomicU8,
    /// Split point as a MIDI note (ADR 0003 §8) — non-automatable shared state.
    split_point: AtomicU8,
}

impl Default for SharedParams {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedParams {
    pub fn new() -> Self {
        Self {
            values: std::array::from_fn(|i| {
                AtomicU32::new(desc_for_clap_id(i).map_or(0.0, |d| d.default).to_bits())
            }),
            gesture: std::array::from_fn(|_| AtomicBool::new(false)),
            key_mode: AtomicU8::new(KeyMode::default() as u8),
            split_point: AtomicU8::new(crate::params::DEFAULT_SPLIT_POINT),
        }
    }

    #[inline]
    pub fn get(&self, index: usize) -> f32 {
        f32::from_bits(self.values[index].load(Ordering::Relaxed))
    }

    /// Store `value` (clamped to the param's range) at CLAP id `index`.
    #[inline]
    pub fn set(&self, index: usize, value: f32) {
        if let Some(d) = desc_for_clap_id(index) {
            self.values[index].store(d.clamp(value).to_bits(), Ordering::Relaxed);
        }
    }

    /// Read by normalized `[0, 1]` position (UI convenience).
    #[inline]
    pub fn get_normalized(&self, index: usize) -> f32 {
        desc_for_clap_id(index).map_or(0.0, |d| d.to_normalized(self.get(index)))
    }

    /// Write from a normalized `[0, 1]` position (UI convenience).
    #[inline]
    pub fn set_normalized(&self, index: usize, n: f32) {
        if let Some(d) = desc_for_clap_id(index) {
            self.set(index, d.from_normalized(n));
        }
    }

    #[inline]
    pub fn gesture(&self, index: usize) -> bool {
        self.gesture[index].load(Ordering::Relaxed)
    }

    /// Mark the start (`true`) or end (`false`) of a UI edit gesture.
    #[inline]
    pub fn set_gesture(&self, index: usize, active: bool) {
        if index < TOTAL_PARAMS {
            self.gesture[index].store(active, Ordering::Relaxed);
        }
    }

    // ── Non-automatable shared state ──────────────────────────────────────────

    #[inline]
    pub fn key_mode(&self) -> KeyMode {
        KeyMode::from_u8(self.key_mode.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_key_mode(&self, mode: KeyMode) {
        self.key_mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Set the key mode from a **discrete UI edit**, performing the one-shot
    /// seed-on-entry copy (ADR 0003 §3): the first transition out of Whole copies
    /// layer A (Upper) → layer B (Lower) so Lower starts equal to Upper and then
    /// diverges. Editing in the store (not just the engine) means the copy
    /// persists and the CLAP layer echoes the seeded Lower values to the host.
    /// `Dual ↔ Split` does not re-seed; state load uses [`Self::set_key_mode`].
    pub fn set_key_mode_seeded(&self, mode: KeyMode) {
        if self.key_mode() == KeyMode::Whole && mode != KeyMode::Whole {
            self.seed_lower_from_upper();
        }
        self.set_key_mode(mode);
    }

    /// Copy every Upper per-patch value into the corresponding Lower slot. The
    /// two per-patch blocks are contiguous CLAP-id ranges (0007), so Lower's id
    /// is its Upper id plus one block width.
    fn seed_lower_from_upper(&self) {
        for p in 0..crate::params::PATCH_COUNT {
            let upper = patch_clap_id(Layer::Upper, PatchParam::from_index(p).unwrap());
            let lower = patch_clap_id(Layer::Lower, PatchParam::from_index(p).unwrap());
            self.values[lower].store(
                self.values[upper].load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
    }

    #[inline]
    pub fn split_point(&self) -> u8 {
        self.split_point.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_split_point(&self, note: u8) {
        self.split_point.store(note.min(127), Ordering::Relaxed);
    }

    // ── Engine / state-blob bridges ───────────────────────────────────────────

    /// Copy the whole store into an engine [`ParamValues`] (audio thread),
    /// routing each CLAP id into its layer/global slot.
    pub fn snapshot_into(&self, params: &mut ParamValues) {
        for i in 0..TOTAL_PARAMS {
            params.set_by_clap_id(i, self.get(i));
        }
    }

    /// Build a [`PluginState`] snapshot for serialization.
    pub fn to_state(&self) -> PluginState {
        let mut params = ParamValues::default();
        self.snapshot_into(&mut params);
        PluginState {
            params,
            key_mode: self.key_mode(),
            split_point: self.split_point(),
        }
    }

    /// Apply a deserialized [`PluginState`] back into the store (state load).
    pub fn restore_from(&self, state: &PluginState) {
        for i in 0..TOTAL_PARAMS {
            self.set(i, state.params.get_by_clap_id(i));
        }
        self.set_key_mode(state.key_mode);
        self.set_split_point(state.split_point);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{GlobalParam, Layer, PatchParam, global_clap_id, patch_clap_id};

    #[test]
    fn defaults_match_table_and_clamp() {
        let s = SharedParams::new();
        let vol = global_clap_id(GlobalParam::MasterVolume);
        assert_eq!(s.get(vol), GlobalParam::MasterVolume.desc().default);
        // Out-of-range writes are clamped to the descriptor range.
        let reso = patch_clap_id(Layer::Upper, PatchParam::Resonance);
        s.set(reso, 5.0);
        assert_eq!(s.get(reso), 1.0);
    }

    #[test]
    fn whole_to_dual_seeds_lower_from_upper_once() {
        let s = SharedParams::new();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        let lo = patch_clap_id(Layer::Lower, PatchParam::Cutoff);
        s.set(up, 1234.0);
        s.set(lo, 9999.0);
        // Whole → Dual seeds Lower from Upper.
        s.set_key_mode_seeded(KeyMode::Dual);
        assert_eq!(s.get(lo), 1234.0, "Lower not seeded from Upper");
        assert_eq!(s.key_mode(), KeyMode::Dual);

        // Diverge Lower, then Dual → Split must NOT re-seed.
        s.set(lo, 555.0);
        s.set_key_mode_seeded(KeyMode::Split);
        assert_eq!(s.get(lo), 555.0, "Dual→Split should not re-seed");
    }

    #[test]
    fn returning_to_whole_then_out_seeds_again() {
        let s = SharedParams::new();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        let lo = patch_clap_id(Layer::Lower, PatchParam::Cutoff);
        s.set_key_mode_seeded(KeyMode::Dual);
        s.set_key_mode_seeded(KeyMode::Whole);
        s.set(up, 4000.0);
        s.set(lo, 1.0);
        // Leaving Whole again re-seeds from the current Upper.
        s.set_key_mode_seeded(KeyMode::Split);
        assert_eq!(s.get(lo), 4000.0);
    }

    #[test]
    fn layers_are_independent() {
        let s = SharedParams::new();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        let lo = patch_clap_id(Layer::Lower, PatchParam::Cutoff);
        s.set(up, 1000.0);
        s.set(lo, 2000.0);
        assert_eq!(s.get(up), 1000.0);
        assert_eq!(s.get(lo), 2000.0);
    }

    #[test]
    fn normalized_roundtrip() {
        let s = SharedParams::new();
        let cutoff = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        s.set_normalized(cutoff, 0.0);
        assert_eq!(s.get(cutoff), PatchParam::Cutoff.desc().min);
        s.set_normalized(cutoff, 1.0);
        assert_eq!(s.get(cutoff), PatchParam::Cutoff.desc().max);
    }

    #[test]
    fn gesture_flag_roundtrips() {
        let s = SharedParams::new();
        assert!(!s.gesture(0));
        s.set_gesture(0, true);
        assert!(s.gesture(0));
    }

    #[test]
    fn key_mode_and_split_default_and_roundtrip() {
        let s = SharedParams::new();
        assert_eq!(s.key_mode(), KeyMode::Whole);
        assert_eq!(s.split_point(), crate::params::DEFAULT_SPLIT_POINT);
        s.set_key_mode(KeyMode::Dual);
        s.set_split_point(72);
        assert_eq!(s.key_mode(), KeyMode::Dual);
        assert_eq!(s.split_point(), 72);
    }

    #[test]
    fn state_roundtrip_through_store() {
        let s = SharedParams::new();
        let up = patch_clap_id(Layer::Upper, PatchParam::Cutoff);
        s.set(up, 4321.0);
        s.set_key_mode(KeyMode::Split);
        s.set_split_point(48);

        let state = s.to_state();
        let s2 = SharedParams::new();
        s2.restore_from(&state);
        assert_eq!(s2.get(up), 4321.0);
        assert_eq!(s2.key_mode(), KeyMode::Split);
        assert_eq!(s2.split_point(), 48);
    }
}
