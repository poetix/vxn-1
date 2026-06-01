//! Bucket-brigade-device (BBD) modelling primitives.
//!
//! Ported from `patches-bundles::patches-vintage` — the continuous-time
//! complex-pole filter banks, the host-rate modulated delay line, and the
//! small support types they need ([`DelayBuffer`] cubic/Thiran reads,
//! [`BoundedRandomWalk`] clock jitter, [`OnePoleLpf`]).
//!
//! VXN1 only needs the *short-delay* BBD regime that chorus lives in
//! (1.6–5.4 ms). At those delays the BBD clock runs ~100–300 kHz, far above
//! Nyquist, so no clock-image folding occurs and the sub-sample H-P engine is
//! pure overhead. [`ModDelayLine`] reproduces the BBD's whole transfer
//! function *except* folding — the shared input/output 4-pole banks, soft
//! bucket saturation and clock jitter — at O(1) per sample, sampling each
//! filter bank once per host sample instead of at sub-sample clock ticks.

use crate::flush_denormal;
use crate::math::fast_tanh;
use std::f32::consts::TAU;

// ── Minimal complex f32 ──────────────────────────────────────────────────────

/// Minimal complex-f32 helper — avoids pulling a dependency for one file.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}

impl Complex32 {
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    pub fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }
    pub fn exp(self) -> Self {
        let m = self.re.exp();
        let (s, c) = self.im.sin_cos();
        Self {
            re: m * c,
            im: m * s,
        }
    }
    /// Multiplicative inverse `1/z`. Undefined at zero.
    pub fn inv(self) -> Self {
        let inv_d = 1.0 / (self.re * self.re + self.im * self.im);
        Self {
            re: self.re * inv_d,
            im: -self.im * inv_d,
        }
    }
}

impl std::ops::Add for Complex32 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }
}
impl std::ops::Mul for Complex32 {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        Self {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}
impl std::ops::Mul<f32> for Complex32 {
    type Output = Self;
    fn mul(self, s: f32) -> Self {
        Self {
            re: self.re * s,
            im: self.im * s,
        }
    }
}
impl std::ops::Div for Complex32 {
    type Output = Self;
    fn div(self, o: Self) -> Self {
        let inv_d = 1.0 / (o.re * o.re + o.im * o.im);
        Self {
            re: (self.re * o.re + self.im * o.im) * inv_d,
            im: (self.im * o.re - self.re * o.im) * inv_d,
        }
    }
}
impl std::ops::Neg for Complex32 {
    type Output = Self;
    fn neg(self) -> Self {
        Self {
            re: -self.re,
            im: -self.im,
        }
    }
}
impl std::ops::AddAssign for Complex32 {
    fn add_assign(&mut self, o: Self) {
        self.re += o.re;
        self.im += o.im;
    }
}

// ── Continuous-time complex pole bank (SoA, vectorisable) ─────────────────────

/// The bank always holds the two conjugate-pole pairs expanded to four complex
/// poles. Fixing the count lets the per-sample loops be `[f32; 4]` arrays the
/// compiler can lower to a single `f32x4` (SSE/NEON 128-bit) lane.
const NPOLES: usize = 4;

/// A bank of complex one-poles `dx/dt = p·x + u(t)`, each advanced once per host
/// sample by the closed-form ODE solution `x[n] = corr·x[n-1] + psi1·u[n]` with
/// `u` held over the sample. The real part of the residue-weighted state sum is
/// the filter output. Real poles come as conjugate pairs so that sum is real.
///
/// Structure-of-arrays layout: every per-pole quantity is a flat `[f32; NPOLES]`
/// so [`advance`](Self::advance) and [`real_output`](Self::real_output) are
/// branch-free fixed-trip loops over contiguous lanes. The four poles are
/// mutually independent within a sample, so this is where the SIMD width lives —
/// the time recurrence itself stays serial (IIR), as it must. Denormal flushing
/// is left to the thread-wide flush-to-zero set at the audio entry, keeping
/// these loops free of the branch a per-lane guard would add.
#[derive(Clone, Debug)]
struct ContinuousPoleBank {
    corr_re: [f32; NPOLES],
    corr_im: [f32; NPOLES],
    psi1_re: [f32; NPOLES],
    psi1_im: [f32; NPOLES],
    r_re: [f32; NPOLES],
    r_im: [f32; NPOLES],
    x_re: [f32; NPOLES],
    x_im: [f32; NPOLES],
}

impl ContinuousPoleBank {
    fn new(poles: [Complex32; NPOLES], residues: [Complex32; NPOLES], sample_rate: f32) -> Self {
        let host_ts = 1.0 / sample_rate;
        let mut b = Self {
            corr_re: [0.0; NPOLES],
            corr_im: [0.0; NPOLES],
            psi1_re: [0.0; NPOLES],
            psi1_im: [0.0; NPOLES],
            r_re: [0.0; NPOLES],
            r_im: [0.0; NPOLES],
            x_re: [0.0; NPOLES],
            x_im: [0.0; NPOLES],
        };
        for k in 0..NPOLES {
            let pole_corr = (poles[k] * host_ts).exp();
            let inv_pole = poles[k].inv();
            let psi1 = Complex32 {
                re: pole_corr.re - 1.0,
                im: pole_corr.im,
            } * inv_pole;
            b.corr_re[k] = pole_corr.re;
            b.corr_im[k] = pole_corr.im;
            b.psi1_re[k] = psi1.re;
            b.psi1_im[k] = psi1.im;
            b.r_re[k] = residues[k].re;
            b.r_im[k] = residues[k].im;
        }
        b
    }

    /// Roll all four poles forward one host sample with input `u` held. The
    /// fixed `0..NPOLES` trip over flat arrays autovectorises to `f32x4`.
    #[inline]
    fn advance(&mut self, u: f32) {
        for k in 0..NPOLES {
            let xr = self.x_re[k];
            let xi = self.x_im[k];
            self.x_re[k] = self.corr_re[k] * xr - self.corr_im[k] * xi + self.psi1_re[k] * u;
            self.x_im[k] = self.corr_re[k] * xi + self.corr_im[k] * xr + self.psi1_im[k] * u;
        }
    }

    fn reset(&mut self) {
        self.x_re = [0.0; NPOLES];
        self.x_im = [0.0; NPOLES];
    }

    /// `Re(Σ r_k · x_k)` — the conjugate pairs make the imaginary part cancel,
    /// so only the real accumulation is computed.
    #[inline]
    fn real_output(&self) -> f32 {
        let mut sum = 0.0_f32;
        for k in 0..NPOLES {
            sum += self.r_re[k] * self.x_re[k] - self.r_im[k] * self.x_im[k];
        }
        sum
    }
}

/// Input / output filter pole set. Two well-damped conjugate-pole pairs giving
/// a non-peaking ~4-pole lowpass rolling off from ~6 kHz. Damped by design so
/// the BBD's input × output transfer stays below unity everywhere. Returns one
/// pole per conjugate pair; the bank adds the twins. Single source of truth —
/// both the input anti-image bank and the output reconstruction bank use it.
fn default_pole_pairs() -> [Complex32; 2] {
    [
        Complex32::new(-30_000.0, 20_000.0),
        Complex32::new(-50_000.0, 30_000.0),
    ]
}

/// Residues (one per pair) normalised so the filter's DC gain is exactly 1.
/// Both raw residues are unit `1+0i`, so `-r/p` reduces to `-1/p` and the
/// doubled-over-halves DC sum collapses to `2·(Re(-1/p₀) + Re(-1/p₁))`.
fn normalised_pair_residues(poles: &[Complex32; 2]) -> [Complex32; 2] {
    let g = 2.0
        * ((-Complex32::new(1.0, 0.0) / poles[0]).re + (-Complex32::new(1.0, 0.0) / poles[1]).re);
    let inv_g = 1.0 / g;
    [Complex32::new(inv_g, 0.0); 2]
}

/// Build a bank carrying the BBD's input/output filter shape: the two pole
/// pairs expanded to full conjugate pairs so the bank's real output is exact.
fn recon_bank(sample_rate: f32) -> ContinuousPoleBank {
    let pairs = default_pole_pairs();
    let res = normalised_pair_residues(&pairs);
    let poles = [pairs[0], pairs[0].conj(), pairs[1], pairs[1].conj()];
    let residues = [res[0], res[0].conj(), res[1], res[1].conj()];
    ContinuousPoleBank::new(poles, residues, sample_rate)
}

// ── One-pole low-pass (trailing reconstruction trim) ──────────────────────────

/// One-pole low-pass: `y[n] = y[n-1] + α (x[n] - y[n-1])`,
/// `α = 1 - exp(-2π fc / sr)`. DC gain unity. The post-BBD reconstruction trim.
#[derive(Default, Clone, Copy)]
pub struct OnePoleLpf {
    alpha: f32,
    y: f32,
}

impl OnePoleLpf {
    pub fn set_cutoff(&mut self, cutoff_hz: f32, sample_rate: f32) {
        self.alpha = 1.0 - (-TAU * cutoff_hz / sample_rate).exp();
    }

    pub fn reset(&mut self) {
        self.y = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.y += self.alpha * (x - self.y);
        self.y
    }
}

// ── Power-of-two ring with cubic / Thiran fractional reads ────────────────────

/// Power-of-two circular buffer with cubic and Thiran fractional reads. `push`
/// pre-increments the write head, so a freshly pushed sample sits at offset 0.
#[derive(Clone)]
struct DelayBuffer {
    data: Box<[f32]>,
    mask: usize,
    write: usize,
}

impl DelayBuffer {
    fn for_duration(max_delay_secs: f32, sample_rate: f32) -> Self {
        let min_samples = ((max_delay_secs * sample_rate).ceil() as usize).max(1);
        let size = min_samples.next_power_of_two();
        Self {
            data: vec![0.0; size].into_boxed_slice(),
            mask: size - 1,
            write: 0,
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.mask + 1
    }

    fn clear(&mut self) {
        self.data.iter_mut().for_each(|s| *s = 0.0);
        self.write = 0;
    }

    #[inline]
    fn push(&mut self, sample: f32) {
        self.write = self.write.wrapping_add(1) & self.mask;
        self.data[self.write] = sample;
    }

    #[inline]
    fn read_at(&self, offset: usize) -> f32 {
        self.data[self.write.wrapping_sub(offset) & self.mask]
    }

    /// Catmull-Rom cubic. `offset` in `[0, capacity - 2]`.
    #[inline]
    fn read_cubic(&self, offset: f32) -> f32 {
        let i = offset as usize;
        let f = offset - i as f32;
        let x0 = self.read_at(i.wrapping_sub(1));
        let x1 = self.read_at(i);
        let x2 = self.read_at(i + 1);
        let x3 = self.read_at(i + 2);
        let a0 = -0.5 * x0 + 1.5 * x1 - 1.5 * x2 + 0.5 * x3;
        let a1 = x0 - 2.5 * x1 + 2.0 * x2 - 0.5 * x3;
        let a2 = -0.5 * x0 + 0.5 * x2;
        let a3 = x1;
        ((a0 * f + a1) * f + a2) * f + a3
    }
}

/// First-order Thiran all-pass interpolation state for a [`DelayBuffer`]. Flat
/// magnitude and group delay across the band — the best fractional read for a
/// smoothly modulated line. Recursive, so reset on a discontinuous delay jump.
#[derive(Default, Clone, Copy)]
struct ThiranInterp {
    y_prev: f32,
}

impl ThiranInterp {
    const FRAC_EPSILON: f32 = 1.0e-3;

    fn reset(&mut self) {
        self.y_prev = 0.0;
    }

    #[inline]
    fn read(&mut self, buf: &DelayBuffer, offset: f32) -> f32 {
        let i = offset as usize;
        let frac = (offset - i as f32).clamp(Self::FRAC_EPSILON, 1.0 - Self::FRAC_EPSILON);
        let a = (1.0 - frac) / (1.0 + frac);
        let x0 = buf.read_at(i);
        let x1 = buf.read_at(i + 1);
        let y = a * (x0 - self.y_prev) + x1;
        self.y_prev = y;
        y
    }
}

// ── Bounded random walk (clock jitter) ────────────────────────────────────────

/// Bounded random walk driven by a 32-bit LCG; each [`advance`](Self::advance)
/// adds `step·noise` and clamps to `[-1, 1]`. Deterministic per seed.
#[derive(Clone)]
struct BoundedRandomWalk {
    rng: u32,
    value: f32,
    step: f32,
}

impl BoundedRandomWalk {
    fn new(seed: u32, step: f32) -> Self {
        Self {
            rng: seed,
            value: 0.0,
            step,
        }
    }

    #[inline(always)]
    fn advance(&mut self) -> f32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (self.rng as i32 as f32) * (1.0 / 2_147_483_648.0);
        self.value = (self.value + noise * self.step).clamp(-1.0, 1.0);
        self.value
    }
}

// ── Fractional-read interpolator selector ─────────────────────────────────────

/// Fractional-read interpolator for the delay tap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Interp {
    /// Catmull-Rom cubic — stateless, near-flat magnitude.
    #[default]
    Cubic,
    /// First-order Thiran all-pass — flat magnitude + group delay.
    Thiran,
}

// ── Host-rate modulated delay line ────────────────────────────────────────────

const JITTER_MAX_DEPTH: f32 = 0.03;
const JITTER_WALK_INTERVAL: u32 = 64;
const JITTER_WALK_STEP: f32 = 0.03;

/// One host-rate modulated delay line mirroring the BBD chain minus folding:
/// input recon bank → saturating write → fractional read → output recon bank →
/// trailing reconstruction one-pole, with optional clock jitter on the read.
#[derive(Clone)]
pub struct ModDelayLine {
    buf: DelayBuffer,
    input_bank: ContinuousPoleBank,
    output_bank: ContinuousPoleBank,
    recon: OnePoleLpf,
    sample_rate: f32,

    interp: Interp,
    thiran: ThiranInterp,

    sat_drive: f32,
    sat_inv_drive: f32,

    jitter_amount: f32,
    jitter_walk: BoundedRandomWalk,
    jitter_counter: u32,
    jitter_value: f32,
}

impl ModDelayLine {
    /// Allocate a line able to hold up to `max_delay_s` of delay. Two extra
    /// samples of headroom cover the cubic interpolator's upper guard tap.
    pub fn new(max_delay_s: f32, sample_rate: f32) -> Self {
        Self {
            buf: DelayBuffer::for_duration(max_delay_s + 2.0 / sample_rate, sample_rate),
            input_bank: recon_bank(sample_rate),
            output_bank: recon_bank(sample_rate),
            recon: OnePoleLpf::default(),
            sample_rate,
            interp: Interp::default(),
            thiran: ThiranInterp::default(),
            sat_drive: 0.0,
            sat_inv_drive: 1.0,
            jitter_amount: 0.0,
            jitter_walk: BoundedRandomWalk::new(0x1BBD_0001, JITTER_WALK_STEP),
            jitter_counter: 0,
            jitter_value: 0.0,
        }
    }

    /// Set the trailing reconstruction one-pole cutoff (per-variant trim).
    pub fn set_recon_cutoff(&mut self, cutoff_hz: f32) {
        self.recon.set_cutoff(cutoff_hz, self.sample_rate);
    }

    /// Select the fractional-read interpolator. Resets the all-pass state so
    /// the switch itself can't click.
    pub fn set_interp(&mut self, interp: Interp) {
        self.interp = interp;
        self.thiran.reset();
    }

    /// Set write soft-saturation drive. `0.0` disables.
    pub fn set_saturation(&mut self, drive: f32) {
        self.sat_drive = drive.max(0.0);
        self.sat_inv_drive = if self.sat_drive > 0.0 {
            1.0 / self.sat_drive
        } else {
            1.0
        };
    }

    /// Clock-jitter amount in `[0, 1]`. `0.0` disables — the walk is not
    /// advanced and the read delay is used unperturbed.
    pub fn set_jitter_amount(&mut self, amount: f32) {
        self.jitter_amount = amount.clamp(0.0, 1.0);
    }

    /// Seed the jitter walk so sibling lines (stereo chorus) decorrelate.
    pub fn set_jitter_seed(&mut self, seed: u32) {
        self.jitter_walk = BoundedRandomWalk::new(seed, JITTER_WALK_STEP);
        self.jitter_counter = 0;
        self.jitter_value = 0.0;
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.input_bank.reset();
        self.output_bank.reset();
        self.recon.reset();
        self.thiran.reset();
        self.jitter_counter = 0;
        self.jitter_value = 0.0;
    }

    /// Process one sample at the commanded delay (seconds). The delay may
    /// change every sample — the fractional read tracks a swept delay cleanly.
    #[inline]
    pub fn process(&mut self, x: f32, delay_s: f32) -> f32 {
        // Clock jitter: slow multiplicative wobble, advanced once per interval.
        let delay_s = if self.jitter_amount > 0.0 {
            if self.jitter_counter == 0 {
                self.jitter_value = self.jitter_walk.advance();
            }
            self.jitter_counter = (self.jitter_counter + 1) % JITTER_WALK_INTERVAL;
            delay_s * (1.0 + self.jitter_value * self.jitter_amount * JITTER_MAX_DEPTH)
        } else {
            delay_s
        };

        // Input anti-aliasing bank → soft-saturating write.
        self.input_bank.advance(x);
        let filtered = self.input_bank.real_output();
        let charge = if self.sat_drive > 0.0 {
            fast_tanh(self.sat_drive * filtered) * self.sat_inv_drive
        } else {
            filtered
        };
        self.buf.push(charge);

        // Fractional read at the commanded delay (offset 0 = freshly pushed).
        let max_offset = self.buf.capacity() as f32 - 2.0;
        let offset = (delay_s * self.sample_rate).clamp(0.0, max_offset);
        let read = match self.interp {
            Interp::Cubic => self.buf.read_cubic(offset),
            Interp::Thiran => self.thiran.read(&self.buf, offset),
        };

        // Output reconstruction bank, then the trailing variant trim.
        self.output_bank.advance(read);
        flush_denormal(self.recon.process(self.output_bank.real_output()))
    }
}

// ── Tapped BBD line (MN3011 multi-tap) ────────────────────────────────────────

/// Number of taps on the [`TappedDelayLine`] — the MN3011's six.
const N_TAPS: usize = 6;

/// MN3011 tap positions in BBD stages, full chain = 3328 stages. "Enharmonic"
/// (mutually unrelated) spacings the chip used to mimic a room's random
/// reflection path-lengths. Normalised in [`TappedDelayLine::new`] to fractions
/// of the commanded full delay so they scale with it exactly as the chip's taps
/// scaled with its clock.
const MN3011_TAP_STAGES: [f32; N_TAPS] = [396.0, 662.0, 1194.0, 1726.0, 2790.0, 3328.0];

/// A single host-rate BBD line read at [`N_TAPS`] fixed fractions of the
/// commanded delay — the MN3011 multi-tap structure feeding `StereoVReverb`.
/// Shares `ModDelayLine`'s input anti-aliasing bank and saturating write; the
/// caller owns the tap mix, output reconstruction, and any feedback. The
/// longest tap (fraction 1.0) sits at the full commanded delay and is the
/// natural recirculation pickoff.
struct TappedDelayLine {
    buf: DelayBuffer,
    input_bank: ContinuousPoleBank,
    sample_rate: f32,
    sat_drive: f32,
    sat_inv_drive: f32,
    tap_frac: [f32; N_TAPS],

    jitter_amount: f32,
    jitter_walk: BoundedRandomWalk,
    jitter_counter: u32,
    jitter_value: f32,
}

impl TappedDelayLine {
    fn new(max_delay_s: f32, sample_rate: f32) -> Self {
        let full = MN3011_TAP_STAGES[N_TAPS - 1];
        Self {
            buf: DelayBuffer::for_duration(max_delay_s + 2.0 / sample_rate, sample_rate),
            input_bank: recon_bank(sample_rate),
            sample_rate,
            sat_drive: 0.0,
            sat_inv_drive: 1.0,
            tap_frac: std::array::from_fn(|k| MN3011_TAP_STAGES[k] / full),
            jitter_amount: 0.0,
            jitter_walk: BoundedRandomWalk::new(0x1BBD_0042, JITTER_WALK_STEP),
            jitter_counter: 0,
            jitter_value: 0.0,
        }
    }

    fn set_saturation(&mut self, drive: f32) {
        self.sat_drive = drive.max(0.0);
        self.sat_inv_drive = if self.sat_drive > 0.0 {
            1.0 / self.sat_drive
        } else {
            1.0
        };
    }

    fn set_jitter_amount(&mut self, amount: f32) {
        self.jitter_amount = amount.clamp(0.0, 1.0);
    }

    fn set_jitter_seed(&mut self, seed: u32) {
        self.jitter_walk = BoundedRandomWalk::new(seed, JITTER_WALK_STEP);
        self.jitter_counter = 0;
        self.jitter_value = 0.0;
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.input_bank.reset();
        self.jitter_counter = 0;
        self.jitter_value = 0.0;
    }

    /// Push one sample (caller has already added any feedback) and read all
    /// [`N_TAPS`] taps at fractions of `full_delay_s`. Cubic reads track a
    /// swept delay cleanly.
    #[inline]
    fn process_tapped(&mut self, x: f32, full_delay_s: f32) -> [f32; N_TAPS] {
        self.input_bank.advance(x);
        let filtered = self.input_bank.real_output();
        let charge = if self.sat_drive > 0.0 {
            fast_tanh(self.sat_drive * filtered) * self.sat_inv_drive
        } else {
            filtered
        };
        self.buf.push(charge);

        let full_delay_s = if self.jitter_amount > 0.0 {
            if self.jitter_counter == 0 {
                self.jitter_value = self.jitter_walk.advance();
            }
            self.jitter_counter = (self.jitter_counter + 1) % JITTER_WALK_INTERVAL;
            full_delay_s * (1.0 + self.jitter_value * self.jitter_amount * JITTER_MAX_DEPTH)
        } else {
            full_delay_s
        };

        let max_offset = self.buf.capacity() as f32 - 2.0;
        let full = full_delay_s * self.sample_rate;
        std::array::from_fn(|k| {
            let offset = (self.tap_frac[k] * full).clamp(0.0, max_offset);
            self.buf.read_cubic(offset)
        })
    }
}

// ── Schroeder allpass diffuser ───────────────────────────────────────────────

/// Single-stage Schroeder allpass: `y[n] = -g·x[n] + z⁻ᴺ·(x[n] + g·y[n])`.
/// Flat magnitude, dispersive phase — smears transients without colouring the
/// steady-state spectrum. Integer delay (sample-accurate; no fractional read
/// needed at the lengths VReverb uses). `g = 0` short-circuits to identity so
/// the diffuser can be cleanly bypassed.
struct AllpassDiffuser {
    buf: DelayBuffer,
    delay_samples: usize,
    g: f32,
}

impl AllpassDiffuser {
    fn new(max_delay_samples: usize) -> Self {
        let size = max_delay_samples.max(1).next_power_of_two();
        Self {
            buf: DelayBuffer {
                data: vec![0.0; size].into_boxed_slice(),
                mask: size - 1,
                write: 0,
            },
            delay_samples: 0,
            g: 0.0,
        }
    }

    fn set_params(&mut self, delay_samples: usize, g: f32) {
        self.delay_samples = delay_samples.min(self.buf.capacity() - 1);
        self.g = g;
    }

    fn reset(&mut self) {
        self.buf.clear();
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        // Bypass: keep bit-exact identity so a zero-diffusion voicing equals
        // the pre-0060 wet path. Buffer is left untouched — Type changes
        // already trigger `StereoVReverb::reset` before bumping diffusion up.
        if self.g == 0.0 {
            return x;
        }
        // y[n] = -g·x[n] + v[n-N], v[n] = x[n] + g·y[n].
        let v_delayed = self.buf.read_at(self.delay_samples);
        let y = -self.g * x + v_delayed;
        let v = x + self.g * y;
        self.buf.push(v);
        y
    }
}

/// Two-stage Schroeder allpass diffuser delays (samples at 48 kHz), L and R
/// channels using different mutually-prime pairs so the two channels decorrelate
/// even before the polarity tap-mix splits them.
const DIFFUSER_DELAYS_L: [usize; 2] = [251, 419];
const DIFFUSER_DELAYS_R: [usize; 2] = [311, 487];
/// Schroeder's stable ceiling for the feedback coefficient.
const DIFFUSION_G_MAX: f32 = 0.7;

// ── StereoVReverb (MN3011 BBD tap-comb) ───────────────────────────────────────

/// Vintage BBD reverb modelled on the Panasonic MN3011: one bucket-brigade line
/// with six "enharmonic" tap positions, two polarity tap-mixes for decorrelated
/// stereo, one feedback path through a damping LPF + `fast_tanh`, with a
/// triangle clock LFO for room-breathing shimmer. Host-rate engine only — sub-
/// sample folding (the `true_bbd` path upstream) is deferred. See E012 / 0055.
///
/// `process_block` returns the *wet* signal; the engine layer does its own
/// dry/wet blend (matches the chorus / delay pattern).
pub struct StereoVReverb {
    line: TappedDelayLine,
    /// Recirculated longest-tap value from the previous sample, already
    /// damped and decay-scaled.
    fb: f32,
    /// One-pole HF damping in the feedback path.
    damp: OnePoleLpf,
    /// Two Schroeder allpass diffusers per channel, in series on the wet
    /// signal between the polarity tap-mix and the output recon bank — out of
    /// the feedback loop so the decay/damp tuning is unchanged.
    diff_l: [AllpassDiffuser; 2],
    diff_r: [AllpassDiffuser; 2],
    out_bank_l: ContinuousPoleBank,
    out_bank_r: ContinuousPoleBank,

    /// Longest-tap delay in seconds.
    full_delay_s: f32,
    decay: f32,

    /// Clock-LFO state.
    lfo_phase: f32,
    lfo_inc: f32,
    /// Effective max swing — already includes the `MOD_MAX_DEPTH` ceiling.
    mod_depth: f32,

    sample_rate: f32,
}

/// BBD bucket-write saturation drive (matches `BbdDevice::BBD_1024` upstream).
const REVERB_SAT_DRIVE: f32 = 1.2;
/// `1/√N_TAPS` — polarity tap-mix gain keeping unit energy across the taps.
const WET_NORM: f32 = 0.408_248_3;
/// Maximum feedback gain (loop-stable ceiling).
const REVERB_DECAY_MAX: f32 = 0.95;
/// Longest-tap delay range, mapped linearly from `size ∈ [0, 1]`.
const FULL_DELAY_MIN_MS: f32 = 35.0;
const FULL_DELAY_MAX_MS: f32 = 180.0;
/// Buffer headroom: longest tap + the LFO's upward swing.
const REVERB_MAX_DELAY_S: f32 = 0.25;
/// Feedback-path damping LPF range. damping=0 → bright, damping=1 → dark.
const DAMP_FC_MIN_HZ: f32 = 1_200.0;
const DAMP_FC_MAX_HZ: f32 = 8_000.0;
/// Clock-LFO rate range, log-mapped from `mod_rate ∈ [0, 1]`.
const MOD_HZ_MIN: f32 = 0.05;
const MOD_HZ_MAX: f32 = 6.0;
/// Max fractional delay swing at `mod_depth = 1` (±15%).
const MOD_MAX_DEPTH: f32 = 0.15;

#[inline]
fn size_to_delay_s(size: f32) -> f32 {
    let s = size.clamp(0.0, 1.0);
    (FULL_DELAY_MIN_MS + (FULL_DELAY_MAX_MS - FULL_DELAY_MIN_MS) * s) * 0.001
}

#[inline]
fn damping_fc_hz(damping: f32) -> f32 {
    let d = damping.clamp(0.0, 1.0);
    let lo = DAMP_FC_MIN_HZ.ln();
    let hi = DAMP_FC_MAX_HZ.ln();
    (hi + (lo - hi) * d).exp()
}

#[inline]
fn mod_rate_hz(mod_rate: f32) -> f32 {
    let r = mod_rate.clamp(0.0, 1.0);
    MOD_HZ_MIN * (MOD_HZ_MAX / MOD_HZ_MIN).powf(r)
}

/// Triangle in `[-1, 1]` from a phase in `[0, 1)`.
#[inline]
fn reverb_triangle(phase: f32) -> f32 {
    4.0 * (phase - 0.5).abs() - 1.0
}

impl StereoVReverb {
    pub fn new(sample_rate: f32, seed: u32) -> Self {
        let mut line = TappedDelayLine::new(REVERB_MAX_DELAY_S, sample_rate);
        line.set_saturation(REVERB_SAT_DRIVE);
        line.set_jitter_seed(seed);
        let mut damp = OnePoleLpf::default();
        damp.set_cutoff(damping_fc_hz(0.5), sample_rate);
        let lfo_inc = mod_rate_hz(0.3) / sample_rate;
        let diff_l = [
            AllpassDiffuser::new(DIFFUSER_DELAYS_L[0]),
            AllpassDiffuser::new(DIFFUSER_DELAYS_L[1]),
        ];
        let diff_r = [
            AllpassDiffuser::new(DIFFUSER_DELAYS_R[0]),
            AllpassDiffuser::new(DIFFUSER_DELAYS_R[1]),
        ];
        Self {
            line,
            fb: 0.0,
            damp,
            diff_l,
            diff_r,
            out_bank_l: recon_bank(sample_rate),
            out_bank_r: recon_bank(sample_rate),
            full_delay_s: size_to_delay_s(0.5),
            decay: 0.6,
            lfo_phase: 0.0,
            lfo_inc,
            mod_depth: 0.2 * MOD_MAX_DEPTH,
            sample_rate,
        }
    }

    /// Set the six underlying knobs. All inputs are `[0, 1]` except `decay`
    /// which is internally clamped to `[0, REVERB_DECAY_MAX]`.
    pub fn set_params(
        &mut self,
        size: f32,
        decay: f32,
        damping: f32,
        mod_rate: f32,
        mod_depth: f32,
        jitter: f32,
    ) {
        self.full_delay_s = size_to_delay_s(size);
        self.decay = decay.clamp(0.0, REVERB_DECAY_MAX);
        self.damp
            .set_cutoff(damping_fc_hz(damping), self.sample_rate);
        self.lfo_inc = mod_rate_hz(mod_rate) / self.sample_rate;
        self.mod_depth = mod_depth.clamp(0.0, 1.0) * MOD_MAX_DEPTH;
        self.line.set_jitter_amount(jitter);
    }

    /// Diffusion amount in `[0, 1]`. `0` is bypass (bit-exact pre-0060 wet
    /// path); `1` is the heaviest setting (g = 0.7, Schroeder's stable ceiling).
    /// Mapped linearly to the feedback coefficient.
    pub fn set_diffusion(&mut self, amount: f32) {
        let g = amount.clamp(0.0, 1.0) * DIFFUSION_G_MAX;
        for k in 0..2 {
            self.diff_l[k].set_params(DIFFUSER_DELAYS_L[k], g);
            self.diff_r[k].set_params(DIFFUSER_DELAYS_R[k], g);
        }
    }

    pub fn reset(&mut self) {
        self.line.clear();
        self.fb = 0.0;
        self.damp.reset();
        for k in 0..2 {
            self.diff_l[k].reset();
            self.diff_r[k].reset();
        }
        self.out_bank_l.reset();
        self.out_bank_r.reset();
        self.lfo_phase = 0.0;
    }

    /// Run one block. `dry` is the mono source. `l` / `r` receive the *wet*
    /// signal — the engine layer does its own dry/wet blend.
    #[inline]
    pub fn process_block(&mut self, dry: &[f32], l: &mut [f32], r: &mut [f32]) {
        let n = dry.len().min(l.len()).min(r.len());
        for i in 0..n {
            self.lfo_phase += self.lfo_inc;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            let full_delay = self.full_delay_s
                * (1.0 + self.mod_depth * reverb_triangle(self.lfo_phase));

            let taps = self.line.process_tapped(dry[i] + self.fb, full_delay);
            let fb_filt = self.damp.process(taps[N_TAPS - 1]);
            self.fb = fast_tanh(self.decay * fb_filt);

            // Two orthogonal polarity tap-mixes → decorrelated stereo.
            let mix_l = taps[0] - taps[1] + taps[2] - taps[3] + taps[4] - taps[5];
            let mix_r = taps[0] + taps[1] - taps[2] - taps[3] + taps[4] + taps[5];
            // Schroeder allpass diffusion: after the polarity tap-mix, before
            // the recon bank. Out of the feedback loop — decay/damping keep
            // their existing meaning.
            let mut wet_l = WET_NORM * mix_l;
            let mut wet_r = WET_NORM * mix_r;
            for k in 0..2 {
                wet_l = self.diff_l[k].process(wet_l);
                wet_r = self.diff_r[k].process(wet_r);
            }
            self.out_bank_l.advance(wet_l);
            self.out_bank_r.advance(wet_r);
            l[i] = flush_denormal(self.out_bank_l.real_output());
            r[i] = flush_denormal(self.out_bank_r.real_output());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;
    const DELAY_S: f32 = 0.002;

    fn rms_gain<F: FnMut(f32) -> f32>(mut step: F, freq: f32) -> f32 {
        let n = SR as usize;
        let warm = n / 4;
        let (mut si, mut so) = (0.0_f64, 0.0_f64);
        for i in 0..n {
            let x = (TAU * freq * (i as f32 / SR)).sin();
            let y = step(x);
            if i >= warm {
                si += (x as f64).powi(2);
                so += (y as f64).powi(2);
            }
        }
        (so / si).sqrt() as f32
    }

    #[test]
    fn dc_gain_is_unity() {
        // ModDelayLine's banks carry no per-lane denormal flush; they rely on
        // the audio thread running under flush-to-zero. Mirror that here.
        crate::enable_flush_to_zero();
        // Both banks and the one-pole are unity-DC; the delay only shifts.
        let mut line = ModDelayLine::new(0.01, SR);
        line.set_recon_cutoff(8_000.0);
        let mut y = 0.0;
        for _ in 0..(SR as usize) {
            y = line.process(1.0, DELAY_S);
        }
        assert!((y - 1.0).abs() < 1e-3, "DC gain should be ~1.0, got {y}");
    }

    #[test]
    fn passband_is_non_peaking() {
        crate::enable_flush_to_zero();
        for &f in &[50.0, 200.0, 1_000.0, 3_000.0, 6_000.0_f32] {
            let mut line = ModDelayLine::new(0.01, SR);
            line.set_recon_cutoff(8_000.0);
            let g = rms_gain(|x| line.process(x, DELAY_S), f);
            assert!(g <= 1.02, "gain at {f} Hz peaked: {g}");
        }
    }

    #[test]
    fn rolls_off_high_frequencies() {
        crate::enable_flush_to_zero();
        let mut line = ModDelayLine::new(0.01, SR);
        line.set_recon_cutoff(8_000.0);
        let lo = rms_gain(|x| line.process(x, DELAY_S), 1_000.0);
        let mut line = ModDelayLine::new(0.01, SR);
        line.set_recon_cutoff(8_000.0);
        let hi = rms_gain(|x| line.process(x, DELAY_S), 14_000.0);
        assert!(hi < lo * 0.5, "expected HF rolloff: 1k={lo}, 14k={hi}");
    }
}

#[cfg(test)]
mod reverb_tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn make() -> StereoVReverb {
        // Banks rely on thread-wide FTZ; mirror the contract here.
        crate::enable_flush_to_zero();
        StereoVReverb::new(SR, 0xBBD0_0040)
    }

    #[test]
    fn wet_decays_to_zero_with_no_input() {
        // Counterpart to upstream's `dry_wet_zero_passes_only_dry` now that
        // mix is external: feed zero, allow any startup transient to die,
        // assert the wet output settles at zero (no self-oscillation in an
        // empty engine).
        let mut rev = make();
        let zeros = [0.0_f32; 64];
        let mut l = [0.0_f32; 64];
        let mut r = [0.0_f32; 64];
        for _ in 0..4_000 {
            rev.process_block(&zeros, &mut l, &mut r);
        }
        for i in 0..64 {
            assert!(l[i].abs() < 1e-6 && r[i].abs() < 1e-6, "wet leaked: l={} r={}", l[i], r[i]);
        }
    }

    #[test]
    fn output_is_bounded_under_sustained_input() {
        let mut rev = make();
        rev.set_params(0.7, 0.9, 0.5, 0.3, 0.2, 0.0);
        let mut dry = [0.0_f32; 64];
        let mut l = [0.0_f32; 64];
        let mut r = [0.0_f32; 64];
        let mut t = 0;
        for _ in 0..(40_000 / 64 + 1) {
            for i in 0..64 {
                let s = (t as f32) / SR;
                dry[i] = 0.5 * (TAU * 440.0 * s).sin();
                t += 1;
            }
            rev.process_block(&dry, &mut l, &mut r);
            for i in 0..64 {
                assert!(
                    l[i].is_finite() && r[i].is_finite() && l[i].abs() < 5.0 && r[i].abs() < 5.0,
                    "diverged at t={t}: l={} r={}", l[i], r[i]
                );
            }
        }
    }

    #[test]
    fn impulse_tail_decays() {
        let mut rev = make();
        // Hold the delay still so the test measures decay, not LFO motion.
        rev.set_params(0.5, 0.6, 0.5, 0.3, 0.0, 0.0);

        // Single-sample impulse.
        let mut dry = [0.0_f32; 1];
        let mut l = [0.0_f32; 1];
        let mut r = [0.0_f32; 1];
        dry[0] = 1.0;
        rev.process_block(&dry, &mut l, &mut r);
        dry[0] = 0.0;

        let mut early_peak = 0.0_f32;
        for _ in 0..((0.3 * SR) as usize) {
            rev.process_block(&dry, &mut l, &mut r);
            let m = l[0].abs().max(r[0].abs());
            if m > early_peak {
                early_peak = m;
            }
        }
        let mut late_peak = 0.0_f32;
        for _ in 0..((0.7 * SR) as usize) {
            rev.process_block(&dry, &mut l, &mut r);
            let m = l[0].abs().max(r[0].abs());
            if m > late_peak {
                late_peak = m;
            }
        }
        assert!(
            late_peak < early_peak,
            "tail should decay: early={early_peak} late={late_peak}"
        );
    }

    #[test]
    fn diffusion_preserves_dry_wet_unity_at_zero() {
        // Two reverbs configured identically. One is left at the default
        // (set_diffusion never called → g = 0 by construction); the other has
        // set_diffusion(0.0) called explicitly. Both must produce bit-identical
        // wet outputs — i.e. amount = 0 is a true bypass equivalent to the
        // pre-0060 wet path.
        let mut a = make();
        let mut b = make();
        a.set_params(0.5, 0.7, 0.5, 0.3, 0.0, 0.0);
        b.set_params(0.5, 0.7, 0.5, 0.3, 0.0, 0.0);
        b.set_diffusion(0.0);

        let mut dry = [0.0_f32; 64];
        let (mut la, mut ra) = ([0.0_f32; 64], [0.0_f32; 64]);
        let (mut lb, mut rb) = ([0.0_f32; 64], [0.0_f32; 64]);
        dry[0] = 1.0;
        a.process_block(&dry, &mut la, &mut ra);
        b.process_block(&dry, &mut lb, &mut rb);
        dry[0] = 0.0;
        for _ in 0..((0.2 * SR / 64.0) as usize) {
            a.process_block(&dry, &mut la, &mut ra);
            b.process_block(&dry, &mut lb, &mut rb);
            for i in 0..64 {
                assert_eq!(la[i].to_bits(), lb[i].to_bits(), "L diverged at i={i}");
                assert_eq!(ra[i].to_bits(), rb[i].to_bits(), "R diverged at i={i}");
            }
        }
    }

    #[test]
    fn diffusion_smooths_impulse_density() {
        // Qualitative: with diffusion = 1, the impulse response in the first
        // 50 ms picks up substantially more zero-crossings than at diffusion
        // = 0 (loose threshold; we're measuring density change, not a number).
        fn zero_crossings(rev: &mut StereoVReverb) -> usize {
            let mut dry = [0.0_f32; 64];
            let mut l = [0.0_f32; 64];
            let mut r = [0.0_f32; 64];
            dry[0] = 1.0;
            rev.process_block(&dry, &mut l, &mut r);
            dry[0] = 0.0;
            let mut prev = l[0];
            let mut crossings = 0;
            // 50 ms ≈ 2400 samples at 48 kHz.
            let total = (0.05 * SR) as usize;
            let mut taken = 1;
            while taken < total {
                rev.process_block(&dry, &mut l, &mut r);
                for i in 0..64 {
                    if taken >= total { break; }
                    if (prev >= 0.0) != (l[i] >= 0.0) {
                        crossings += 1;
                    }
                    prev = l[i];
                    taken += 1;
                }
            }
            crossings
        }

        let mut dry_rev = make();
        dry_rev.set_params(0.5, 0.6, 0.5, 0.3, 0.0, 0.0);
        dry_rev.set_diffusion(0.0);
        let dry_cx = zero_crossings(&mut dry_rev);

        let mut wet_rev = make();
        wet_rev.set_params(0.5, 0.6, 0.5, 0.3, 0.0, 0.0);
        wet_rev.set_diffusion(1.0);
        let wet_cx = zero_crossings(&mut wet_rev);

        assert!(
            wet_cx > dry_cx * 2,
            "expected denser response under diffusion: dry={dry_cx} wet={wet_cx}"
        );
    }

    #[test]
    fn diffusion_does_not_blow_up() {
        let mut rev = make();
        rev.set_params(0.7, 0.9, 0.5, 0.3, 0.2, 0.0);
        rev.set_diffusion(1.0);
        let mut dry = [0.0_f32; 64];
        let mut l = [0.0_f32; 64];
        let mut r = [0.0_f32; 64];
        let mut t = 0;
        for _ in 0..(40_000 / 64 + 1) {
            for i in 0..64 {
                let s = (t as f32) / SR;
                dry[i] = 0.5 * (TAU * 440.0 * s).sin();
                t += 1;
            }
            rev.process_block(&dry, &mut l, &mut r);
            for i in 0..64 {
                assert!(
                    l[i].is_finite() && r[i].is_finite() && l[i].abs() < 5.0 && r[i].abs() < 5.0,
                    "diverged at t={t}: l={} r={}", l[i], r[i]
                );
            }
        }
    }

    #[test]
    fn taps_decorrelate_stereo() {
        let mut rev = make();
        rev.set_params(0.5, 0.7, 0.5, 0.3, 0.2, 0.0);
        let mut dry = [0.0_f32; 1];
        let mut l = [0.0_f32; 1];
        let mut r = [0.0_f32; 1];
        let mut max_diff = 0.0_f32;
        for i in 0..20_000 {
            dry[0] = if i < 64 { 0.8 } else { 0.0 };
            rev.process_block(&dry, &mut l, &mut r);
            max_diff = max_diff.max((l[0] - r[0]).abs());
        }
        assert!(max_diff > 1e-3, "L/R never decorrelated: max_diff={max_diff}");
    }
}
