//! Macro mapping for the reverb panel. The faceplate exposes Type + Depth +
//! Mix; the engine consumes the six underlying knobs the BBD tap-comb actually
//! needs (`size, decay, damping, mod_rate, mod_depth, jitter`). `jitter` stays
//! pinned at 0 — not part of the voicing surface in v1 (see E012 §Out of
//! scope).
//!
//! The mapping is a `pub const` table per type: each type fixes
//! `decay / damping / mod_rate / mod_depth` and a `[size_min, size_max]`
//! window that `depth ∈ [0, 1]` lerps inside. Plate is the most polite, Large
//! the most drenched. Smaller voicings carry a faster, more lively wobble;
//! Hall/Large drift slowly so the "room breathing" is subliminal rather than
//! a chorus effect.

/// Reverb voicing type. The user-facing Type knob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ReverbType {
    Plate = 0,
    Room = 1,
    Hall = 2,
    Large = 3,
}

impl ReverbType {
    pub const COUNT: usize = 4;
    pub const ALL: [ReverbType; Self::COUNT] = [
        ReverbType::Plate,
        ReverbType::Room,
        ReverbType::Hall,
        ReverbType::Large,
    ];

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub fn from_index(i: usize) -> ReverbType {
        Self::ALL[i.min(Self::COUNT - 1)]
    }
}

/// One row of the macro table. `mod_rate` is in the same `[0, 1]` space the
/// DSP layer log-maps to `[0.05, 6.0]` Hz — see `StereoVReverb::set_params`.
#[derive(Clone, Copy, Debug)]
struct MacroRow {
    size_min: f32,
    size_max: f32,
    decay: f32,
    damp: f32,
    mod_rate: f32,
    mod_depth: f32,
    /// Schroeder allpass diffusion in `[0, 1]`. Plate stays dry-ish to keep
    /// the metallic ring; Hall/Large drench so listeners get a smooth density
    /// instead of comb-flutter.
    diffusion: f32,
}

/// Per-type voicing rows. Indexed by `ReverbType::index()`. `mod_rate` values
/// resolve (under the log map) to roughly: Plate ~0.6 Hz, Room ~0.3 Hz, Hall
/// ~0.15 Hz, Large ~0.08 Hz.
const MACRO_TABLE: [MacroRow; ReverbType::COUNT] = [
    // Plate: small, bright, lively wobble.
    MacroRow { size_min: 0.10, size_max: 0.30, decay: 0.55, damp: 0.30, mod_rate: 0.52, mod_depth: 0.10, diffusion: 0.30 },
    // Room: medium, neutral.
    MacroRow { size_min: 0.25, size_max: 0.55, decay: 0.65, damp: 0.50, mod_rate: 0.37, mod_depth: 0.20, diffusion: 0.55 },
    // Hall: long, dark, subtle motion.
    MacroRow { size_min: 0.50, size_max: 0.80, decay: 0.78, damp: 0.65, mod_rate: 0.23, mod_depth: 0.12, diffusion: 0.75 },
    // Large: very long, dark, gentle drift.
    MacroRow { size_min: 0.70, size_max: 1.00, decay: 0.88, damp: 0.75, mod_rate: 0.10, mod_depth: 0.15, diffusion: 0.85 },
];

/// Resolved underlying knobs for `StereoVReverb::set_params`. `jitter` is not
/// in here — it is pinned to 0 in the engine caller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReverbVoicing {
    pub size: f32,
    pub decay: f32,
    pub damping: f32,
    pub mod_rate: f32,
    pub mod_depth: f32,
    pub diffusion: f32,
}

/// Resolve `(type, depth)` to the underlying voicing. `depth` is clamped to
/// `[0, 1]` and lerps `size` between the type's `[size_min, size_max]` window.
#[inline]
pub fn reverb_macro(t: ReverbType, depth: f32) -> ReverbVoicing {
    let row = MACRO_TABLE[t.index()];
    let d = depth.clamp(0.0, 1.0);
    let size = row.size_min + (row.size_max - row.size_min) * d;
    ReverbVoicing {
        size,
        decay: row.decay,
        damping: row.damp,
        mod_rate: row.mod_rate,
        mod_depth: row.mod_depth,
        diffusion: row.diffusion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macro_size_lerps_within_type_range() {
        for t in ReverbType::ALL {
            let row = MACRO_TABLE[t.index()];
            let v0 = reverb_macro(t, 0.0);
            let vm = reverb_macro(t, 0.5);
            let v1 = reverb_macro(t, 1.0);
            assert!((v0.size - row.size_min).abs() < 1e-6, "{t:?} depth=0");
            let mid = 0.5 * (row.size_min + row.size_max);
            assert!((vm.size - mid).abs() < 1e-6, "{t:?} depth=0.5");
            assert!((v1.size - row.size_max).abs() < 1e-6, "{t:?} depth=1");
        }
    }

    #[test]
    fn macro_fixed_per_type() {
        for t in ReverbType::ALL {
            let row = MACRO_TABLE[t.index()];
            for &d in &[0.0_f32, 0.25, 0.5, 0.75, 1.0] {
                let v = reverb_macro(t, d);
                assert_eq!(v.decay, row.decay, "{t:?}");
                assert_eq!(v.damping, row.damp, "{t:?}");
                assert_eq!(v.mod_rate, row.mod_rate, "{t:?}");
                assert_eq!(v.mod_depth, row.mod_depth, "{t:?}");
                assert_eq!(v.diffusion, row.diffusion, "{t:?}");
            }
        }
    }

    #[test]
    fn macro_clamps_depth_out_of_range() {
        let v_lo = reverb_macro(ReverbType::Room, -0.5);
        let v_hi = reverb_macro(ReverbType::Room, 1.5);
        assert_eq!(v_lo.size, MACRO_TABLE[1].size_min);
        assert_eq!(v_hi.size, MACRO_TABLE[1].size_max);
    }

    #[test]
    fn reverb_type_from_index_roundtrips() {
        for t in ReverbType::ALL {
            assert_eq!(ReverbType::from_index(t.index()), t);
            }
        assert_eq!(ReverbType::from_index(ReverbType::COUNT), ReverbType::Large);
        assert_eq!(ReverbType::from_index(999), ReverbType::Large);
    }
}
