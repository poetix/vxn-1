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
pub mod bbd;
pub mod chorus;
pub mod delay;
pub mod halfband;
pub mod hpf;
pub mod ladder;
pub mod lfo;
pub mod math;
pub mod oscillator;
pub mod phase;
pub mod poly;
pub mod smoothing;

/// Channels (DSP voices) per layer. The poly kernels are sized to this: one
/// homogeneous layer renders together, which is what the vectorised lane loop
/// needs (ADR 0003 §10). Fixed so per-voice arrays live on the stack and the
/// compiler can unroll/vectorise voice loops.
pub const CHANNELS_PER_LAYER: usize = 8;

/// Maximum total polyphony across both always-present layers (ADR 0003 §2).
pub const MAX_VOICES: usize = 2 * CHANNELS_PER_LAYER;

/// Maximum oversampling factor for the synthesis path. Bounds the size of the
/// oversampled scratch buffer (`CONTROL_BLOCK * MAX_OVERSAMPLE`).
pub const MAX_OVERSAMPLE: usize = 8;

/// Engine control-block size in samples. Modulation and coefficients are
/// recomputed once per block; the per-sample inner loop runs this many times.
/// 32 @ 48 kHz ≈ 0.67 ms — well below any audible zipper threshold for the
/// modulation depths VXN1 uses.
pub const CONTROL_BLOCK: usize = 32;

pub use adsr::{AdsrCore, AdsrShape, AdsrStage};
pub use chorus::StereoChorus;
pub use delay::{DelayLine, StereoDelay};
pub use halfband::{HalfbandFir, Oversampler};
pub use hpf::{HpfKernel, PolyHpf};
pub use ladder::{LadderCoeffs, LadderKernel, LadderVariant};
pub use lfo::{LfoCore, LfoShape};
pub use math::{fast_exp2, fast_sine, fast_tanh, lookup_sine, xorshift64};
pub use oscillator::{Oscillator, Waveform};
pub use phase::{MonoPhaseAccumulator, polyblep};
pub use poly::{PolyLadder, PolyOscillator, poly_ring_mod};
pub use smoothing::{Smoothed, ms_to_samples, one_pole_coeff};

/// Flush x86/ARM denormals-to-zero on the current thread, without restoring the
/// previous mode. Denormal arithmetic can cost 100× and silently wreck
/// real-time deadlines in filter/delay feedback paths.
///
/// Prefer [`ScopedFlushToZero`] at the top of each `process()` call: it is
/// robust to the host running `process` on a different thread than `activate`,
/// and restores the host's FP mode on the way out so it doesn't perturb other
/// plugins in the chain. This bare setter is kept for tests and one-shot setup.
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

/// RAII guard that enables flush-to-zero (and denormals-are-zero on x86) for the
/// current thread, restoring the previous FP control word on drop.
///
/// Construct it at the top of every audio `process()` call and hold it for the
/// block's duration:
///
/// ```ignore
/// let _ftz = ScopedFlushToZero::new();
/// // … render the block; all SSE/NEON ops run flush-to-zero …
/// ```
///
/// Setting per-process (rather than once in `activate`) is the robust choice:
/// the FP control word is thread-local, and a host may legitimately call
/// `process` on a different thread than `activate`. Restoring on drop keeps us
/// from changing the FP mode seen by the host or other plugins after we return.
#[must_use = "FTZ is restored when the guard drops; bind it for the whole block"]
pub struct ScopedFlushToZero {
    #[cfg(target_arch = "x86_64")]
    prev: u32,
    #[cfg(target_arch = "aarch64")]
    prev: u64,
}

impl ScopedFlushToZero {
    #[inline]
    pub fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::{
                _MM_FLUSH_ZERO_ON, _MM_GET_FLUSH_ZERO_MODE, _MM_SET_FLUSH_ZERO_MODE,
            };
            // Save the FTZ mode, then enable it. (FTZ flushes denormal results,
            // which is all our filter state needs — same scope as the bare
            // `enable_flush_to_zero` setter.)
            let prev = unsafe { _MM_GET_FLUSH_ZERO_MODE() };
            unsafe { _MM_SET_FLUSH_ZERO_MODE(_MM_FLUSH_ZERO_ON) };
            Self { prev }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // Save FPCR, then set FZ (bit 24).
            let prev: u64;
            unsafe {
                std::arch::asm!("mrs {}, fpcr", out(reg) prev, options(nomem, nostack));
                std::arch::asm!("msr fpcr, {}", in(reg) prev | (1 << 24), options(nomem, nostack));
            }
            Self { prev }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            // No portable denormal-flush control; nothing to save or restore.
            Self {}
        }
    }
}

impl Default for ScopedFlushToZero {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopedFlushToZero {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_MM_SET_FLUSH_ZERO_MODE(self.prev);
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) self.prev, options(nomem, nostack));
        }
    }
}

/// Flush a single denormal `f32` to zero. Per-sample guard for filter/delay
/// feedback state that decays into the denormal range, complementing the
/// thread-wide [`enable_flush_to_zero`] (which not every host honours).
#[inline]
pub fn flush_denormal(x: f32) -> f32 {
    if !x.is_normal() && x != 0.0 { 0.0 } else { x }
}

/// Reference frequency for V/oct: MIDI note 0 (C-1) ≈ 8.1758 Hz.
pub const MIDI_0_HZ: f32 = 8.175_799;

/// Convert a MIDI note number (with fractional cents/bend) to frequency in Hz.
#[inline]
pub fn note_to_hz(note: f32) -> f32 {
    MIDI_0_HZ * fast_exp2(note / 12.0)
}
