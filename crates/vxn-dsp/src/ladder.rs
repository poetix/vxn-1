//! Zero-delay-feedback 4-pole transistor-ladder lowpass. Adapted from
//! `patches-dsp::ladder`, with the per-sample coefficient ramp removed —
//! VXN1 recomputes coefficients once per control block (see crate docs), so
//! the kernel just holds frozen coefficients and runs the integrator chain.
//!
//! Zavalishin TPT one-pole per stage, global `tanh` on the feedback path,
//! one-sample-delayed feedback (Huovilainen simplification). Self-oscillates
//! at `resonance = 1.0` (`k = 4`).

use crate::math::fast_tanh;
use std::f32::consts::PI;

/// Coefficient voicing preset.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LadderVariant {
    /// Sharp resonance peak, unity input scale, unmodified cutoff.
    Sharp,
    /// Softer top (HF loss) and bass compression under resonance.
    Smooth,
}

#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Frozen ladder coefficients for one control block.
#[derive(Copy, Clone, Debug)]
pub struct LadderCoeffs {
    /// One-pole stage gain in `(0, 1)`.
    pub g: f32,
    /// Global feedback factor in `[0, 4]` (self-oscillation at 4).
    pub k: f32,
    /// Input drive applied before the feedback `tanh`.
    pub drive: f32,
    /// Input scale (bass compression under resonance for the smooth voicing).
    pub scale: f32,
}

impl LadderCoeffs {
    pub fn new(cutoff_hz: f32, sample_rate: f32, resonance: f32, drive: f32, variant: LadderVariant) -> Self {
        let fc = cutoff_hz.clamp(5.0, sample_rate * 0.45);
        let wd = (PI * fc / sample_rate).tan();
        let g_raw = wd / (1.0 + wd);
        let g = match variant {
            LadderVariant::Sharp => g_raw,
            LadderVariant::Smooth => g_raw * 0.95,
        }
        .clamp(1.0e-5, 0.999);
        let k = 4.0 * resonance.clamp(0.0, 1.0);
        let scale = match variant {
            LadderVariant::Sharp => 1.0,
            LadderVariant::Smooth => 1.0 - 0.0875 * k,
        };
        Self { g, k, drive: drive.max(0.0), scale }
    }
}

/// Single-voice ladder kernel.
#[derive(Clone)]
pub struct LadderKernel {
    g: f32,
    k: f32,
    drive: f32,
    scale: f32,
    s: [f32; 4],
    y4_prev: f32,
}

impl LadderKernel {
    pub fn new() -> Self {
        Self { g: 0.5, k: 0.0, drive: 1.0, scale: 1.0, s: [0.0; 4], y4_prev: 0.0 }
    }

    /// Replace coefficients (call once per control block).
    #[inline]
    pub fn set_coeffs(&mut self, c: LadderCoeffs) {
        self.g = c.g;
        self.k = c.k;
        self.drive = c.drive;
        self.scale = c.scale;
    }

    pub fn reset(&mut self) {
        self.s = [0.0; 4];
        self.y4_prev = 0.0;
    }

    /// Run one sample, return the 4th-stage (24 dB/oct) output.
    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let g = self.g;
        let u = fast_tanh(self.drive * x * self.scale - self.k * self.y4_prev);
        let mut y = u;
        for i in 0..4 {
            let v = (y - self.s[i]) * g;
            let yn = v + self.s[i];
            self.s[i] = sanitize(yn + v);
            y = yn;
        }
        self.y4_prev = sanitize(y);
        y
    }
}

impl Default for LadderKernel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_dc_and_attenuates_hf() {
        let sr = 48_000.0;
        let mut k = LadderKernel::new();
        k.set_coeffs(LadderCoeffs::new(1000.0, sr, 0.0, 1.0, LadderVariant::Sharp));
        // DC settles near input. Use a small-signal input so the input tanh
        // stays linear (tanh(0.05) ≈ 0.04996); the four LP stages pass DC at
        // unity, so the steady-state gain is ~1.
        let x = 0.05;
        let mut last = 0.0;
        for _ in 0..2000 {
            last = k.tick(x);
        }
        assert!((last / x - 1.0).abs() < 0.02, "dc gain {}", last / x);

        // Nyquist-ish input is heavily attenuated.
        k.reset();
        let mut peak = 0.0f32;
        for i in 0..2000 {
            let s = if i % 2 == 0 { x } else { -x };
            peak = peak.max(k.tick(s).abs());
        }
        assert!(peak < 0.3 * x, "hf leakage {}", peak / x);
    }

    #[test]
    fn stable_at_high_resonance() {
        let sr = 48_000.0;
        let mut k = LadderKernel::new();
        k.set_coeffs(LadderCoeffs::new(2000.0, sr, 1.0, 1.0, LadderVariant::Sharp));
        let mut peak = 0.0f32;
        for i in 0..48_000 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let y = k.tick(x);
            assert!(y.is_finite());
            peak = peak.max(y.abs());
        }
        assert!(peak < 10.0, "self-osc blew up: {peak}");
    }
}
