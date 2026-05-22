//! Stereo chorus: a small modulated delay line per channel with quadrature
//! LFOs, in the classic Juno/string-machine style.
//!
//! This is a clean modulated-delay implementation reusing [`DelayLine`]. A
//! future upgrade can swap the delay core for the bucket-brigade (BBD) model
//! from `patches-bundles::patches-vintage` (clock-aliasing image-fold + bucket
//! saturation) for more authentic vintage character; the interface here is
//! designed to accommodate that drop-in later.

use crate::delay::DelayLine;
use crate::math::lookup_sine;

/// Base delay range for the modulated taps, in milliseconds.
const MIN_DELAY_MS: f32 = 5.0;
const MAX_DELAY_MS: f32 = 25.0;

#[derive(Clone)]
pub struct StereoChorus {
    sample_rate: f32,
    left: DelayLine,
    right: DelayLine,
    lfo_phase: f32,
    lfo_inc: f32,
    // Control-block parameters.
    depth: f32,    // 0..1 → modulation excursion
    center_ms: f32,
    mix: f32,
}

impl StereoChorus {
    pub fn new(sample_rate: f32) -> Self {
        let max = (sample_rate * (MAX_DELAY_MS + 5.0) * 0.001) as usize + 4;
        Self {
            sample_rate,
            left: DelayLine::new(max),
            right: DelayLine::new(max),
            lfo_phase: 0.0,
            lfo_inc: 0.5 / sample_rate,
            depth: 0.5,
            center_ms: (MIN_DELAY_MS + MAX_DELAY_MS) * 0.5,
            mix: 0.5,
        }
    }

    pub fn clear(&mut self) {
        self.left.clear();
        self.right.clear();
        self.lfo_phase = 0.0;
    }

    /// Set parameters for the next control block. `rate_hz` typically 0.1–6 Hz,
    /// `depth` and `mix` in `[0, 1]`.
    pub fn set_params(&mut self, rate_hz: f32, depth: f32, mix: f32) {
        self.lfo_inc = rate_hz.clamp(0.01, 12.0) / self.sample_rate;
        self.depth = depth.clamp(0.0, 1.0);
        self.mix = mix.clamp(0.0, 1.0);
    }

    /// Process one stereo sample. The left and right taps are modulated in
    /// quadrature (90° apart) for stereo width.
    #[inline]
    pub fn process(&mut self, in_l: f32, in_r: f32) -> (f32, f32) {
        self.lfo_phase += self.lfo_inc;
        if self.lfo_phase >= 1.0 {
            self.lfo_phase -= 1.0;
        }
        let mod_l = lookup_sine(self.lfo_phase);
        let mod_r = lookup_sine((self.lfo_phase + 0.25).fract());

        let excursion_ms = self.depth * (MAX_DELAY_MS - MIN_DELAY_MS) * 0.5;
        let ms_to_samp = self.sample_rate * 0.001;
        let dl = (self.center_ms + mod_l * excursion_ms) * ms_to_samp;
        let dr = (self.center_ms + mod_r * excursion_ms) * ms_to_samp;

        self.left.write(in_l);
        self.right.write(in_r);
        let wet_l = self.left.read(dl);
        let wet_r = self.right.read(dr);

        let m = self.mix;
        (in_l * (1.0 - m) + wet_l * m, in_r * (1.0 - m) + wet_r * m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_finite_and_passes_signal() {
        let sr = 48_000.0;
        let mut c = StereoChorus::new(sr);
        c.set_params(1.0, 0.7, 0.5);
        let mut energy = 0.0f32;
        for i in 0..48_000 {
            let x = lookup_sine((i as f32 * 220.0 / sr).fract());
            let (l, r) = c.process(x, x);
            assert!(l.is_finite() && r.is_finite());
            energy += l.abs();
        }
        assert!(energy > 100.0, "chorus produced near-silence");
    }
}
