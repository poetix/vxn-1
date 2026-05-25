//! Host-tempo sync for the LFOs (E004 / 0015).
//!
//! When an LFO's sync is on, its rate knob no longer means free-running Hz —
//! it selects a **musical subdivision** locked to the host tempo. The knob's
//! normalised position picks a subdivision from [`SUBDIVISIONS`] (coarse →
//! fine), and [`synced_hz`] resolves that to an actual Hz from the current BPM.
//!
//! The LFO core stays Hz-driven (ADR 0002 §Consequences): sync is purely a rate
//! computation here, isolated from [`vxn_dsp::LfoCore`].

/// Fallback tempo when the host provides none (no `HAS_TEMPO`). A sane musical
/// default so a synced LFO never stalls or NaNs absent transport.
pub const DEFAULT_TEMPO_BPM: f32 = 120.0;

/// One tempo-sync subdivision: its label and its length in **beats per LFO
/// cycle** (quarter note = 1 beat). Straight = base, dotted = ×1.5, triplet =
/// ×2/3.
#[derive(Clone, Copy, Debug)]
pub struct Subdivision {
    pub label: &'static str,
    pub beats: f32,
}

const fn s(label: &'static str, beats: f32) -> Subdivision {
    Subdivision { label, beats }
}

const T: f32 = 2.0 / 3.0;

/// Subdivisions coarse → fine, each as straight / dotted / triplet, 1/1 … 1/32.
pub static SUBDIVISIONS: [Subdivision; 18] = [
    s("1/1", 4.0),
    s("1/1.", 4.0 * 1.5),
    s("1/1T", 4.0 * T),
    s("1/2", 2.0),
    s("1/2.", 2.0 * 1.5),
    s("1/2T", 2.0 * T),
    s("1/4", 1.0),
    s("1/4.", 1.0 * 1.5),
    s("1/4T", 1.0 * T),
    s("1/8", 0.5),
    s("1/8.", 0.5 * 1.5),
    s("1/8T", 0.5 * T),
    s("1/16", 0.25),
    s("1/16.", 0.25 * 1.5),
    s("1/16T", 0.25 * T),
    s("1/32", 0.125),
    s("1/32.", 0.125 * 1.5),
    s("1/32T", 0.125 * T),
];

/// Map an LFO rate knob's normalised position `[0, 1]` to a subdivision index.
#[inline]
pub fn index_from_norm(norm: f32) -> usize {
    let last = SUBDIVISIONS.len() - 1;
    (norm.clamp(0.0, 1.0) * last as f32).round() as usize
}

/// Resolve a subdivision (by index) at `tempo_bpm` to an LFO frequency in Hz.
/// Caller clamps to the LFO's valid Hz range (`LfoCore::set_rate` does).
#[inline]
pub fn synced_hz(tempo_bpm: f32, index: usize) -> f32 {
    let beats = SUBDIVISIONS[index.min(SUBDIVISIONS.len() - 1)].beats;
    // beats/sec ÷ beats/cycle = cycles/sec (Hz).
    (tempo_bpm / 60.0) / beats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_subdivisions_match_beat_math() {
        // 1/4 cycles once per beat: at 120 BPM that's 2 Hz; at 90, 1.5 Hz.
        let q = SUBDIVISIONS.iter().position(|s| s.label == "1/4").unwrap();
        assert!((synced_hz(120.0, q) - 2.0).abs() < 1e-5);
        assert!((synced_hz(90.0, q) - 1.5).abs() < 1e-5);
        // 1/8 is twice as fast.
        let e = SUBDIVISIONS.iter().position(|s| s.label == "1/8").unwrap();
        assert!((synced_hz(90.0, e) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn dotted_and_triplet_scale_the_straight_rate() {
        let q = SUBDIVISIONS.iter().position(|s| s.label == "1/4").unwrap();
        let qd = SUBDIVISIONS.iter().position(|s| s.label == "1/4.").unwrap();
        let qt = SUBDIVISIONS.iter().position(|s| s.label == "1/4T").unwrap();
        for bpm in [90.0_f32, 140.0] {
            let straight = synced_hz(bpm, q);
            // Dotted is 1.5× longer → 2/3 the rate; triplet 2/3 longer → 1.5×.
            assert!((synced_hz(bpm, qd) - straight / 1.5).abs() < 1e-4, "dotted {bpm}");
            assert!((synced_hz(bpm, qt) - straight * 1.5).abs() < 1e-4, "triplet {bpm}");
        }
    }

    #[test]
    fn norm_maps_across_the_whole_table() {
        assert_eq!(index_from_norm(0.0), 0);
        assert_eq!(index_from_norm(1.0), SUBDIVISIONS.len() - 1);
        // Clamped, never out of bounds.
        assert_eq!(index_from_norm(-1.0), 0);
        assert_eq!(index_from_norm(2.0), SUBDIVISIONS.len() - 1);
    }
}
