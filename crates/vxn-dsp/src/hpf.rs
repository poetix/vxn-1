//! One-pole (−6 dB/oct) high-pass filter, placed pre-VCF in the JP-8 topology
//! (Source Mixer → HPF → VCF → VCA). Thins body / removes DC below the cutoff.
//!
//! Topological-preserving-transform one-pole (Zavalishin): compute the one-pole
//! *lowpass* `lp` and return `x − lp`, which is the complementary high-pass.
//! The coefficient `a = g/(1+g)` with `g = tan(π·fc/sr)` is the same mapping the
//! ladder uses per stage, so HPF and VCF share a coefficient convention.
//!
//! Coefficients are frozen per control block (set once via `set_cutoff`); the
//! HPF cutoff is not a modulation destination, so no per-sample ramp is needed
//! (unlike [`crate::PolyOtaLadder`], whose cutoff is modulated).

use crate::CHANNELS_PER_LAYER;
use std::f32::consts::PI;

const N: usize = CHANNELS_PER_LAYER;

#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Map a cutoff in Hz to the TPT one-pole coefficient `a = g/(1+g)`.
#[inline]
fn coeff(cutoff_hz: f32, sample_rate: f32) -> f32 {
    let fc = cutoff_hz.clamp(5.0, sample_rate * 0.45);
    let wd = (PI * fc / sample_rate).tan();
    (wd / (1.0 + wd)).clamp(1.0e-5, 0.999)
}

/// Single-voice one-pole high-pass kernel.
#[derive(Clone)]
pub struct HpfKernel {
    a: f32,
    s: f32,
}

impl HpfKernel {
    pub fn new() -> Self {
        Self { a: 0.0, s: 0.0 }
    }

    /// Set the cutoff (call once per control block).
    #[inline]
    pub fn set_cutoff(&mut self, cutoff_hz: f32, sample_rate: f32) {
        self.a = coeff(cutoff_hz, sample_rate);
    }

    pub fn reset(&mut self) {
        self.s = 0.0;
    }

    /// Run one sample; returns the high-passed value `x − lowpass(x)`.
    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let v = (x - self.s) * self.a;
        let lp = v + self.s;
        self.s = sanitize(lp + v);
        x - lp
    }
}

impl Default for HpfKernel {
    fn default() -> Self {
        Self::new()
    }
}

/// 16-voice structure-of-arrays one-pole high-pass, mirroring [`crate::PolyOtaLadder`]'s
/// shape (`set_cutoff(v, …)` / `process(&in, &mut out)`). Per-voice coefficients
/// (the cutoff is global today, but the SoA form keeps it in the same lane loop
/// as the ladder).
#[derive(Clone)]
pub struct PolyHpf {
    a: [f32; N],
    s: [f32; N],
}

impl Default for PolyHpf {
    fn default() -> Self {
        Self::new()
    }
}

impl PolyHpf {
    pub fn new() -> Self {
        Self {
            a: [0.0; N],
            s: [0.0; N],
        }
    }

    pub fn reset(&mut self) {
        self.s = [0.0; N];
    }

    /// Set voice `v`'s cutoff (call once per control block).
    #[inline]
    pub fn set_cutoff(&mut self, v: usize, cutoff_hz: f32, sample_rate: f32) {
        self.a[v] = coeff(cutoff_hz, sample_rate);
    }

    /// Set the same cutoff for every voice, computing the coefficient once.
    /// The HPF cutoff is global (not a per-voice modulation destination), so
    /// this avoids recomputing `tan()` per lane.
    #[inline]
    pub fn set_cutoff_all(&mut self, cutoff_hz: f32, sample_rate: f32) {
        self.a = [coeff(cutoff_hz, sample_rate); N];
    }

    /// One sample per voice: `out[v] = highpass(x[v])`.
    #[inline]
    pub fn process(&mut self, x: &[f32; N], out: &mut [f32; N]) {
        for v in 0..N {
            let a = self.a[v];
            let vv = (x[v] - self.s[v]) * a;
            let lp = vv + self.s[v];
            self.s[v] = sanitize(lp + vv);
            out[v] = x[v] - lp;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state gain at a frequency, as output peak / input peak. Both
    /// peaks are taken from the same sampled sine, so undersampling of a
    /// high-frequency tone cancels out of the ratio.
    fn peak_gain(cutoff: f32, freq: f32, sr: f32) -> f32 {
        let mut k = HpfKernel::new();
        k.set_cutoff(cutoff, sr);
        let mut out_peak = 0.0f32;
        let mut in_peak = 0.0f32;
        let n = 20_000;
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f32 / sr).sin();
            let y = k.tick(x);
            // Ignore the initial transient.
            if i > n / 2 {
                out_peak = out_peak.max(y.abs());
                in_peak = in_peak.max(x.abs());
            }
        }
        out_peak / in_peak
    }

    #[test]
    fn attenuates_dc_and_lows_passes_highs() {
        let sr = 48_000.0;
        // DC is fully removed.
        let mut k = HpfKernel::new();
        k.set_cutoff(500.0, sr);
        let mut last = 0.0;
        for _ in 0..5000 {
            last = k.tick(1.0);
        }
        assert!(last.abs() < 1e-3, "DC not blocked: {last}");

        // A frequency well below cutoff is attenuated; well above passes ~unity.
        let low = peak_gain(500.0, 50.0, sr);
        let high = peak_gain(500.0, 8000.0, sr);
        assert!(low < 0.2, "low not attenuated: {low}");
        assert!(high > 0.9, "high not passed: {high}");
    }

    #[test]
    fn default_low_cutoff_is_near_transparent() {
        // At the default 20 Hz cutoff, mid/high content passes essentially
        // untouched ("off").
        let sr = 48_000.0;
        let g = peak_gain(20.0, 1000.0, sr);
        assert!(
            (g - 1.0).abs() < 0.02,
            "20 Hz HPF not transparent at 1 kHz: {g}"
        );
    }

    #[test]
    fn poly_lane0_matches_scalar() {
        let sr = 48_000.0;
        let mut kern = HpfKernel::new();
        kern.set_cutoff(800.0, sr);
        let mut poly = PolyHpf::new();
        poly.set_cutoff(0, 800.0, sr);
        let mut out = [0.0; N];
        let mut max_diff = 0.0f32;
        for i in 0..4800 {
            let x = (2.0 * PI * 300.0 * i as f32 / sr).sin();
            let xs = [x; N];
            poly.process(&xs, &mut out);
            let s = kern.tick(x);
            max_diff = max_diff.max((out[0] - s).abs());
        }
        assert!(max_diff < 1e-6, "poly vs scalar HPF diff {max_diff}");
    }
}
