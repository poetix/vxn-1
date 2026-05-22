//! Fast math approximations. Copied from `patches-dsp::approximate` with the
//! consumers VXN1 needs. Error bounds and rationale are preserved in the docs.

use std::f32::consts::TAU;

/// Rational (Padé degree-5/6) approximation to `tanh`, saturating to ±1 for
/// `|x| ≥ 2.5`. Exact at 0, monotone, RMS error < 0.05 over [−3, 3].
#[inline(always)]
pub fn fast_tanh(x: f32) -> f32 {
    if x >= 2.5 {
        return 1.0;
    }
    if x <= -2.5 {
        return -1.0;
    }
    let x2 = x * x;
    let x4 = x2 * x2;
    let x6 = x4 * x2;
    x * (10395.0 + 1260.0 * x2 + 21.0 * x4)
        / (10395.0 + 4725.0 * x2 + 210.0 * x4 + 4.0 * x6)
}

static SINE_TABLE: std::sync::LazyLock<Vec<f32>> =
    std::sync::LazyLock::new(|| (0..1024).map(|i| (i as f32 / 1024.0 * TAU).sin()).collect());

/// Linearly-interpolated 1024-point sine table lookup. `phase` in `[0, 1)`.
/// RMS error ≈ 1e-6.
#[inline(always)]
pub fn lookup_sine(phase: f32) -> f32 {
    let index = phase * 1024.0;
    let index_whole = index as usize;
    let index_frac = index - (index_whole as f32);
    let a = SINE_TABLE[index_whole & 1023];
    let b = SINE_TABLE[(index_whole + 1) & 1023];
    a + (b - a) * index_frac
}

/// Bhaskara-I + Moser polynomial sine. `phase` in `[0, 1)`. Max abs err ≈ 0.001.
#[inline(always)]
pub fn fast_sine(phase: f32) -> f32 {
    let x1 = phase - 0.5;
    let x2 = x1 * 16.0 * (x1.abs() - 0.5);
    x2 + 0.225 * x2 * (x2.abs() - 1.0)
}

/// Fast `2^x`. Max relative error < 1e-4 over [−10, 10]. Reconstructs the
/// integer part via the IEEE-754 exponent field, 5th-order poly on the
/// fractional part.
#[inline]
pub fn fast_exp2(x: f32) -> f32 {
    if x.is_nan() {
        return f32::NAN;
    }
    if x >= 128.0 {
        return f32::INFINITY;
    }
    if x <= -150.0 {
        return 0.0;
    }
    let i = x.floor();
    let f = x - i;

    let c1 = std::f32::consts::LN_2;
    let c2 = 0.240_226_5_f32;
    let c3 = 0.055_504_11_f32;
    let c4 = 0.009_618_13_f32;
    let c5 = 0.001_333_36_f32;
    let poly = (((c5 * f + c4) * f + c3) * f + c2) * f + c1;
    let frac = poly * f + 1.0;

    let ei = (i as i32) + 127;
    if ei <= 0 {
        return 0.0;
    }
    if ei >= 255 {
        return f32::INFINITY;
    }
    let bits = (ei as u32) << 23;
    f32::from_bits(bits) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_key_points() {
        for (p, e) in [(0.0, 0.0), (0.25, 1.0), (0.5, 0.0), (0.75, -1.0)] {
            assert!((fast_sine(p) - e).abs() < 0.002, "fast_sine({p})");
            assert!((lookup_sine(p) - e).abs() < 0.002, "lookup_sine({p})");
        }
    }

    #[test]
    fn exp2_matches_std() {
        for x in [-10.0, -1.0, 0.0, 0.5, 1.0, 7.0f32] {
            let rel = ((fast_exp2(x) - x.exp2()) / x.exp2()).abs();
            assert!(rel < 1e-4, "fast_exp2({x}) rel err {rel}");
        }
    }

    #[test]
    fn tanh_saturates_and_odd() {
        assert_eq!(fast_tanh(0.0), 0.0);
        assert!((fast_tanh(10.0) - 1.0).abs() < 1e-6);
        assert!((fast_tanh(-10.0) + 1.0).abs() < 1e-6);
        for i in 1..=25 {
            let x = i as f32 * 0.1;
            assert!((fast_tanh(x) + fast_tanh(-x)).abs() < 1e-6);
        }
    }
}
