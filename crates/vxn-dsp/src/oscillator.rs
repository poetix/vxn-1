//! Anti-aliased waveform oscillator built on [`MonoPhaseAccumulator`].
//!
//! Saw and pulse use PolyBLEP at their discontinuities; triangle is the
//! integral of a band-limited square (cheap leaky integrator of the BLEP'd
//! square) — but for VXN1 v1 we use the naive triangle, which has gently
//! rolled-off harmonics and aliases far less than saw/pulse. Sine is table
//! lookup (effectively alias-free).

use crate::math::lookup_sine;
use crate::phase::{MonoPhaseAccumulator, polyblep};

/// Selectable oscillator waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    /// Square / variable-width pulse (duty set via `pulse_width`).
    Pulse,
}

impl Waveform {
    pub const ALL: [Waveform; 4] = [Waveform::Sine, Waveform::Triangle, Waveform::Saw, Waveform::Pulse];

    pub fn label(self) -> &'static str {
        match self {
            Waveform::Sine => "Sine",
            Waveform::Triangle => "Triangle",
            Waveform::Saw => "Saw",
            Waveform::Pulse => "Pulse",
        }
    }
}

/// A single oscillator. Holds its own phase; the caller sets frequency
/// (via `set_increment`) and reads samples with [`next`](Self::next).
#[derive(Clone)]
pub struct Oscillator {
    acc: MonoPhaseAccumulator,
    /// Pulse duty cycle in `(0, 1)`. Ignored by non-pulse waveforms.
    pub pulse_width: f32,
}

impl Oscillator {
    pub fn new() -> Self {
        Self { acc: MonoPhaseAccumulator::new(), pulse_width: 0.5 }
    }

    pub fn reset(&mut self) {
        self.acc.reset();
    }

    /// Set the per-sample phase increment (`freq_hz / sample_rate`).
    #[inline]
    pub fn set_increment(&mut self, increment: f32) {
        self.acc.set_increment(increment);
    }

    /// Produce the next sample and advance the phase.
    #[inline]
    pub fn next(&mut self, wave: Waveform) -> f32 {
        let phase = self.acc.phase;
        let dt = self.acc.phase_increment;
        let out = match wave {
            Waveform::Sine => lookup_sine(phase),
            Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Waveform::Saw => {
                // Naive ramp minus the BLEP residual at the wrap discontinuity.
                (2.0 * phase - 1.0) - polyblep(phase, dt)
            }
            Waveform::Pulse => {
                let pw = self.pulse_width.clamp(0.01, 0.99);
                let naive = if phase < pw { 1.0 } else { -1.0 };
                // Rising edge at phase=0, falling edge at phase=pw.
                let mut v = naive + polyblep(phase, dt);
                let pf = (phase - pw + 1.0).fract();
                v -= polyblep(pf, dt);
                v
            }
        };
        self.acc.advance();
        out
    }
}

impl Default for Oscillator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn all_waveforms_bounded_and_nonzero() {
        for wave in Waveform::ALL {
            let mut osc = Oscillator::new();
            osc.set_increment(440.0 / 48000.0);
            let buf: Vec<f32> = (0..4800).map(|_| osc.next(wave)).collect();
            assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= 2.0), "{wave:?} bounds");
            assert!(rms(&buf) > 0.1, "{wave:?} produced near-silence");
        }
    }

    #[test]
    fn sine_is_approximately_unit_amplitude() {
        let mut osc = Oscillator::new();
        osc.set_increment(100.0 / 48000.0);
        let buf: Vec<f32> = (0..4800).map(|_| osc.next(Waveform::Sine)).collect();
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!((peak - 1.0).abs() < 0.02, "sine peak {peak}");
    }
}
