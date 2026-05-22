//! VXN1 DSP kernels.
//!
//! Pure, allocation-free-on-the-hot-path DSP building blocks for the VXN1
//! synthesizer. Adapted from the `patches` / `patches-bundles` codebases and
//! rewritten for VXN1's static signal flow.
//!
//! ## Processing model
//!
//! Kernels expose per-sample `next()` / `tick()` methods. The recurrences
//! (phase accumulation, envelope state machines, ladder integrators) are
//! inherently serial, so the per-sample form is the natural one and is kept
//! bit-faithful to the originals.
//!
//! The *block* optimisation lives one level up in `vxn-engine`: control-rate
//! quantities (modulation, filter coefficients, smoothed parameters) are
//! recomputed once per fixed control block ([`CONTROL_BLOCK`]) and the inner
//! per-sample loop stays branch-light.
//!
//! Nothing here depends on the plugin framework or the UI.

pub mod adsr;
pub mod chorus;
pub mod delay;
pub mod halfband;
pub mod ladder;
pub mod lfo;
pub mod math;
pub mod noise;
pub mod oscillator;
pub mod phase;
pub mod smoothing;

/// Maximum polyphony. Fixed so per-voice arrays can live on the stack and the
/// compiler can unroll/vectorise voice loops.
pub const MAX_VOICES: usize = 16;

/// Maximum oversampling factor for the synthesis path. Bounds the size of the
/// oversampled scratch buffer (`CONTROL_BLOCK * MAX_OVERSAMPLE`).
pub const MAX_OVERSAMPLE: usize = 4;

/// Engine control-block size in samples. Modulation and coefficients are
/// recomputed once per block; the per-sample inner loop runs this many times.
/// 32 @ 48 kHz ≈ 0.67 ms — well below any audible zipper threshold for the
/// modulation depths VXN1 uses.
pub const CONTROL_BLOCK: usize = 32;

pub use adsr::{AdsrCore, AdsrShape, AdsrStage};
pub use chorus::StereoChorus;
pub use delay::{DelayLine, StereoDelay};
pub use halfband::{HalfbandFir, Oversampler};
pub use ladder::{LadderCoeffs, LadderKernel, LadderVariant};
pub use lfo::{LfoCore, LfoShape};
pub use math::{fast_exp2, fast_sine, fast_tanh, lookup_sine};
pub use noise::{BrownFilter, NoiseColor, NoiseSource, PinkFilter, xorshift64};
pub use oscillator::{Oscillator, Waveform};
pub use phase::{MonoPhaseAccumulator, polyblep};
pub use smoothing::{Smoothed, ms_to_samples, one_pole_coeff};

/// Flush x86/ARM denormals-to-zero on the current thread. Call once at the top
/// of the audio thread's processing entry point. Denormal arithmetic can cost
/// 100× and silently wreck real-time deadlines in filter/delay feedback paths.
#[inline]
pub fn enable_flush_to_zero() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_MM_FLUSH_ZERO_ON, _MM_SET_FLUSH_ZERO_MODE};
        _MM_SET_FLUSH_ZERO_MODE(_MM_FLUSH_ZERO_ON);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // FPCR bit 24 (FZ): flush-to-zero.
        let mut fpcr: u64;
        std::arch::asm!("mrs {}, fpcr", out(reg) fpcr);
        fpcr |= 1 << 24;
        std::arch::asm!("msr fpcr, {}", in(reg) fpcr);
    }
}

/// Reference frequency for V/oct: MIDI note 0 (C-1) ≈ 8.1758 Hz.
pub const MIDI_0_HZ: f32 = 8.175_799;

/// Convert a MIDI note number (with fractional cents/bend) to frequency in Hz.
#[inline]
pub fn note_to_hz(note: f32) -> f32 {
    MIDI_0_HZ * fast_exp2(note / 12.0)
}
