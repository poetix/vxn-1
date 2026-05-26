//! Halfband FIR decimator and a 2×/4×/8× oversampling helper.
//!
//! `HalfbandFir` is copied from `patches-dsp::halfband`: a 33-tap symmetric
//! linear-phase halfband filter (8 non-zero off-centre taps + centre), every
//! other tap zero by the halfband property. `process(a, b)` consumes two
//! oversampled samples and returns one band-limited, decimated sample
//! (>60 dB stopband, ~0.1 dB passband ripple, group delay 16 oversampled
//! samples).
//!
//! VXN1 generates the voice path directly at the oversampled rate (the
//! oscillators run with a smaller phase increment), so only *decimation* is
//! needed — there is no interpolation stage.

/// Non-zero off-centre taps for the default 33-tap halfband FIR.
pub const DEFAULT_TAPS: [f32; 8] = [
    -0.00188788,
    0.00386248,
    -0.00824247,
    0.01594711,
    -0.02867656,
    0.05071856,
    -0.09801591,
    0.31594176,
];
pub const DEFAULT_CENTRE: f32 = 0.500_705_8;

/// Symmetric linear-phase halfband FIR used as a 2× decimator.
#[derive(Clone)]
pub struct HalfbandFir {
    taps: Vec<f32>,
    delay: Vec<f32>,
    pos: usize,
    mask: usize,
    centre: f32,
    midpoint_offset: usize,
}

impl HalfbandFir {
    /// Group delay in oversampled samples (half the filter order).
    pub const GROUP_DELAY_OVERSAMPLED: usize = 16;

    pub fn new(taps: Vec<f32>, centre: f32) -> Self {
        let taps_len = taps.len();
        let len = (taps_len * 4 + 2).next_power_of_two();
        Self {
            taps,
            delay: vec![0.0; len],
            pos: 0,
            mask: len - 1,
            centre,
            midpoint_offset: len - (taps_len * 2),
        }
    }

    pub fn reset(&mut self) {
        self.delay.iter_mut().for_each(|s| *s = 0.0);
        self.pos = 0;
    }

    /// Decimate two oversampled input samples into one output sample.
    #[inline]
    pub fn process(&mut self, first: f32, second: f32) -> f32 {
        let n_taps = self.taps.len();
        let mask = self.mask;

        let newest = self.push_sample(first);
        self.push_sample(second);

        let center_idx = (newest + self.midpoint_offset) & mask;
        let mut acc = self.centre * self.delay[center_idx];

        let mut offset_r = (center_idx + 1) & mask;
        let mut offset_l = (center_idx + mask) & mask;

        for t in (0..n_taps).rev() {
            acc += self.taps[t] * (self.delay[offset_l] + self.delay[offset_r]);
            offset_r = (offset_r + 2) & mask;
            offset_l = (offset_l + mask - 1) & mask;
        }
        acc
    }

    #[inline]
    fn push_sample(&mut self, x: f32) -> usize {
        let idx = self.pos;
        self.delay[idx] = x;
        self.pos = (self.pos + 1) & self.mask;
        idx
    }
}

impl Default for HalfbandFir {
    fn default() -> Self {
        Self::new(DEFAULT_TAPS.to_vec(), DEFAULT_CENTRE)
    }
}

/// 2× / 4× / 8× oversampling decimator. Holds three cascaded halfband stages,
/// each a 2:1 step run at successively lower rates: 8× runs stage A (8→4),
/// stage B (4→2), stage C (2→1); 4× uses A (4→2) then B (2→1); 2× uses A only.
/// A given stage always operates at the same rate regardless of factor, so its
/// filter state stays coherent.
#[derive(Clone)]
pub struct Oversampler {
    stage_a: HalfbandFir,
    stage_b: HalfbandFir,
    stage_c: HalfbandFir,
}

impl Default for Oversampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Oversampler {
    pub fn new() -> Self {
        Self {
            stage_a: HalfbandFir::default(),
            stage_b: HalfbandFir::default(),
            stage_c: HalfbandFir::default(),
        }
    }

    pub fn reset(&mut self) {
        self.stage_a.reset();
        self.stage_b.reset();
        self.stage_c.reset();
    }

    /// Decimate `input` (length `output.len() * factor`) into `output`.
    /// `factor` must be 1, 2, 4 or 8. For 1× this is a straight copy.
    pub fn decimate(&mut self, input: &[f32], output: &mut [f32], factor: usize) {
        match factor {
            2 => {
                for (i, out) in output.iter_mut().enumerate() {
                    *out = self.stage_a.process(input[2 * i], input[2 * i + 1]);
                }
            }
            4 => {
                for (i, out) in output.iter_mut().enumerate() {
                    let base = 4 * i;
                    let a = self.stage_a.process(input[base], input[base + 1]);
                    let b = self.stage_a.process(input[base + 2], input[base + 3]);
                    *out = self.stage_b.process(a, b);
                }
            }
            8 => {
                for (i, out) in output.iter_mut().enumerate() {
                    let base = 8 * i;
                    // 8 → 4 (stage A)
                    let a0 = self.stage_a.process(input[base], input[base + 1]);
                    let a1 = self.stage_a.process(input[base + 2], input[base + 3]);
                    let a2 = self.stage_a.process(input[base + 4], input[base + 5]);
                    let a3 = self.stage_a.process(input[base + 6], input[base + 7]);
                    // 4 → 2 (stage B)
                    let b0 = self.stage_b.process(a0, a1);
                    let b1 = self.stage_b.process(a2, a3);
                    // 2 → 1 (stage C)
                    *out = self.stage_c.process(b0, b1);
                }
            }
            _ => {
                output.copy_from_slice(&input[..output.len()]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_passes_through_2x() {
        let mut os = Oversampler::new();
        let input = [1.0f32; 64];
        let mut output = [0.0f32; 32];
        // Flush, then measure.
        for _ in 0..4 {
            os.decimate(&input, &mut output, 2);
        }
        let tail = output[output.len() - 4..].iter().sum::<f32>() / 4.0;
        assert!((tail - 1.0).abs() < 0.01, "2x DC gain {tail}");
    }

    #[test]
    fn nyquist_rejected_2x() {
        // Alternating ±1 at the oversampled rate is the oversampled Nyquist;
        // a halfband decimator should crush it.
        let mut os = Oversampler::new();
        let input: [f32; 64] = std::array::from_fn(|i| if i % 2 == 0 { 1.0 } else { -1.0 });
        let mut output = [0.0f32; 32];
        for _ in 0..6 {
            os.decimate(&input, &mut output, 2);
        }
        let peak = output[output.len() - 8..]
            .iter()
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak < 0.05, "2x Nyquist leakage {peak}");
    }

    #[test]
    fn dc_passes_through_4x() {
        let mut os = Oversampler::new();
        let input = [1.0f32; 128];
        let mut output = [0.0f32; 32];
        for _ in 0..6 {
            os.decimate(&input, &mut output, 4);
        }
        let tail = output[output.len() - 4..].iter().sum::<f32>() / 4.0;
        assert!((tail - 1.0).abs() < 0.02, "4x DC gain {tail}");
    }

    #[test]
    fn dc_passes_through_8x() {
        let mut os = Oversampler::new();
        let input = [1.0f32; 256];
        let mut output = [0.0f32; 32];
        for _ in 0..6 {
            os.decimate(&input, &mut output, 8);
        }
        let tail = output[output.len() - 4..].iter().sum::<f32>() / 4.0;
        assert!((tail - 1.0).abs() < 0.02, "8x DC gain {tail}");
    }

    #[test]
    fn nyquist_rejected_8x() {
        // Alternating ±1 at the 8× rate is the oversampled Nyquist; the cascade
        // should crush it back to the base rate.
        let mut os = Oversampler::new();
        let input: [f32; 256] = std::array::from_fn(|i| if i % 2 == 0 { 1.0 } else { -1.0 });
        let mut output = [0.0f32; 32];
        for _ in 0..6 {
            os.decimate(&input, &mut output, 8);
        }
        let peak = output[output.len() - 8..]
            .iter()
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak < 0.05, "8x Nyquist leakage {peak}");
    }

    #[test]
    fn passthrough_1x_is_identity() {
        let mut os = Oversampler::new();
        let input: [f32; 32] = std::array::from_fn(|i| i as f32);
        let mut output = [0.0f32; 32];
        os.decimate(&input, &mut output, 1);
        assert_eq!(input, output);
    }
}
