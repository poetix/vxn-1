//! Phase accumulator and PolyBLEP. Copied from `patches-dsp::oscillator`.

/// Single-voice normalised phase accumulator in `[0.0, 1.0)`.
///
/// Knows nothing about frequency or sample rate — set the per-sample increment
/// directly (`freq_hz / sample_rate`).
#[derive(Clone)]
pub struct MonoPhaseAccumulator {
    pub phase: f32,
    pub phase_increment: f32,
}

impl MonoPhaseAccumulator {
    pub fn new() -> Self {
        Self { phase: 0.0, phase_increment: 0.0 }
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// Set the per-sample increment, clamped below Nyquist so [`advance`] can
    /// wrap with a single conditional subtraction.
    #[inline]
    pub fn set_increment(&mut self, increment: f32) {
        self.phase_increment = increment.min(0.999_999);
    }

    /// Advance and wrap to `[0.0, 1.0)`.
    #[inline]
    pub fn advance(&mut self) {
        let phase = self.phase + self.phase_increment;
        let wrap = if phase >= 1.0 { 1.0 } else { 0.0 };
        self.phase = phase - wrap;
    }
}

impl Default for MonoPhaseAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// PolyBLEP correction for normalised phase `t ∈ [0, 1)` and increment `dt`.
///
/// Smooths the discontinuity near `t = 0` (rising) and `t = 1` (falling).
/// Effective only when `dt < 0.5`.
#[inline]
pub fn polyblep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        2.0 * t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + 2.0 * t + 1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_once_per_period_at_440hz() {
        let mut acc = MonoPhaseAccumulator::new();
        acc.set_increment(440.0 / 44100.0);
        let mut wraps = 0;
        let mut prev = acc.phase;
        for _ in 0..102 {
            acc.advance();
            if acc.phase < prev {
                wraps += 1;
            }
            prev = acc.phase;
        }
        assert_eq!(wraps, 1);
    }

    #[test]
    fn polyblep_zero_in_interior() {
        assert_eq!(polyblep(0.5, 0.01), 0.0);
    }
}
