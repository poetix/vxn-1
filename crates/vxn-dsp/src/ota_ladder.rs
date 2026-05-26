//! R3109/IR3109-style OTA-C ladder lowpass — a Roland/Juno-flavoured filter.
//!
//! Four TPT one-pole stages like [`crate::ladder`], but the nonlinearity lives
//! **inside each integrator** (a per-stage `tanh` on the integrator input)
//! rather than on the global feedback sum. That matches the softer, more
//! distributed saturation of OTA-C filter chips (IR3109, CEM3320, …) and gives
//! a cleaner, more sinusoidal self-oscillation than the Moog-style transistor
//! ladder in [`crate::ladder`].
//!
//! Differences from [`crate::ladder::LadderKernel`]:
//!
//! * Per-stage `tanh`, not a single global pre-feedback `tanh`.
//! * **No** resonance-dependent input attenuation — Juno-style filters don't
//!   thin the bass under high resonance, so there is no `scale` term and no
//!   Sharp/Smooth voicing axis. The voicing knob here is the output slope.
//! * Selectable 2-pole (12 dB/oct) or 4-pole (24 dB/oct) output tap
//!   ([`OtaPoles`]). The resonance feedback loop is always taken from the 4th
//!   stage, so the filter self-oscillates identically at `k ≈ 4` in either
//!   mode; the 2-pole tap simply reads stage 1's output and inherits the
//!   resonance peak shaped by the full 4-pole loop.
//!
//! Frozen-coefficient kernel, matching VXN1's per-control-block model (see
//! crate docs); the engine recomputes coefficients once per block. The poly
//! sibling [`crate::poly::PolyOtaLadder`] additionally ramps them per sample.

use crate::math::fast_tanh;
use std::f32::consts::PI;

/// Filter order: which stage feeds the output tap. Feedback is always taken
/// from the 4th stage, so resonance/self-oscillation is identical in both.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OtaPoles {
    /// 12 dB/oct — output read from stage 1.
    Two,
    /// 24 dB/oct — output read from stage 3.
    Four,
}

impl OtaPoles {
    /// Index into the 4-stage output array for this mode.
    #[inline]
    pub fn output_tap(self) -> usize {
        match self {
            OtaPoles::Two => 1,
            OtaPoles::Four => 3,
        }
    }
}

#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

#[inline]
pub(crate) fn compute_g(cutoff_hz: f32, sample_rate: f32) -> f32 {
    let fc = cutoff_hz.clamp(5.0, sample_rate * 0.45);
    let wd = (PI * fc / sample_rate).tan();
    (wd / (1.0 + wd)).clamp(1.0e-5, 0.999)
}

/// Frozen OTA-ladder coefficients for one control block.
#[derive(Copy, Clone, Debug)]
pub struct OtaLadderCoeffs {
    /// TPT one-pole stage gain in `(0, 1)`.
    pub g: f32,
    /// Global feedback factor in `[0, 4]` (self-oscillation at 4).
    pub k: f32,
    /// Input drive applied before stage 0's `tanh`.
    pub drive: f32,
}

impl OtaLadderCoeffs {
    /// `resonance` is taken in `[0, 1]` and scaled to the `[0, 4]` feedback
    /// range internally (self-oscillation at `resonance = 1.0`), matching the
    /// call convention of [`crate::ladder::LadderCoeffs::new`].
    #[inline]
    pub fn new(cutoff_hz: f32, sample_rate: f32, resonance: f32, drive: f32) -> Self {
        Self {
            g: compute_g(cutoff_hz, sample_rate),
            k: 4.0 * resonance.clamp(0.0, 1.0),
            drive: drive.max(0.0),
        }
    }
}

/// Single-voice OTA-ladder kernel. Frozen coefficients (set once per block).
#[derive(Clone)]
pub struct OtaLadderKernel {
    g: f32,
    k: f32,
    drive: f32,
    poles: OtaPoles,
    s: [f32; 4],
    y4_prev: f32,
}

impl OtaLadderKernel {
    pub fn new() -> Self {
        Self {
            g: 0.5,
            k: 0.0,
            drive: 1.0,
            poles: OtaPoles::Four,
            s: [0.0; 4],
            y4_prev: 0.0,
        }
    }

    /// Replace coefficients (call once per control block).
    #[inline]
    pub fn set_coeffs(&mut self, c: OtaLadderCoeffs) {
        self.g = c.g;
        self.k = c.k;
        self.drive = c.drive;
    }

    /// Change output-tap mode. The feedback path is unchanged, so the filter
    /// keeps ringing identically — only the output slope shifts.
    #[inline]
    pub fn set_poles(&mut self, poles: OtaPoles) {
        self.poles = poles;
    }

    pub fn poles(&self) -> OtaPoles {
        self.poles
    }

    pub fn reset(&mut self) {
        self.s = [0.0; 4];
        self.y4_prev = 0.0;
    }

    /// Run one sample, return the selected output tap.
    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let g = self.g;
        let fed = self.drive * x - self.k * self.y4_prev;
        let mut input = fed;
        let mut stages = [0.0f32; 4];
        for (i, stage) in stages.iter_mut().enumerate() {
            let u = fast_tanh(input);
            let v = (u - self.s[i]) * g;
            let yn = v + self.s[i];
            self.s[i] = sanitize(yn + v);
            *stage = yn;
            input = yn;
        }
        self.y4_prev = sanitize(stages[3]);
        stages[self.poles.output_tap()]
    }
}

impl Default for OtaLadderKernel {
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
        let mut k = OtaLadderKernel::new();
        k.set_coeffs(OtaLadderCoeffs::new(1000.0, sr, 0.0, 1.0));
        let x = 0.05;
        let mut last = 0.0;
        for _ in 0..2000 {
            last = k.tick(x);
        }
        assert!((last / x - 1.0).abs() < 0.02, "dc gain {}", last / x);

        k.reset();
        let mut peak = 0.0f32;
        for i in 0..2000 {
            let s = if i % 2 == 0 { x } else { -x };
            peak = peak.max(k.tick(s).abs());
        }
        assert!(peak < 0.3 * x, "hf leakage {}", peak / x);
    }

    #[test]
    fn two_pole_tap_is_brighter_than_four_pole() {
        // Same coefficients, different output tap: the 12 dB/oct tap must let
        // more HF through than the 24 dB/oct tap. Use a sub-Nyquist sine well
        // above the cutoff — a pure-Nyquist test is degenerate because the
        // bilinear one-pole has an exact zero at Nyquist (both taps → 0).
        let sr = 48_000.0;
        let f = 6_000.0;
        let c = OtaLadderCoeffs::new(1000.0, sr, 0.0, 1.0);
        let hf_energy = |poles| {
            let mut k = OtaLadderKernel::new();
            k.set_coeffs(c);
            k.set_poles(poles);
            let mut e = 0.0f32;
            for i in 0..4000 {
                let s = 0.1 * (2.0 * PI * f * i as f32 / sr).sin();
                let y = k.tick(s);
                if i > 2000 {
                    e += y * y;
                }
            }
            e
        };
        assert!(hf_energy(OtaPoles::Two) > 4.0 * hf_energy(OtaPoles::Four));
    }

    #[test]
    fn stable_at_high_resonance() {
        let sr = 48_000.0;
        let mut k = OtaLadderKernel::new();
        k.set_coeffs(OtaLadderCoeffs::new(2000.0, sr, 1.0, 1.0));
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
