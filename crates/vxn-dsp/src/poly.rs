//! Structure-of-arrays poly kernels for the synthesis hot path.
//!
//! Each kernel holds `[f32; CHANNELS_PER_LAYER]` state and processes one layer's
//! channels per sample in a branchless loop the compiler auto-vectorises (NEON
//! is 4-wide f32, so 8 channels = 2 SIMD lanes deep). Waveform / noise colour /
//! filter variant are *per-layer* parameters, hoisted outside the lane loop —
//! the inner loop has no data-dependent branches. A heterogeneous second layer
//! is simply a second kernel instance with its own hoisted globals.
//!
//! Mirrors the design of `patches-dsp`'s poly kernels. The mono kernels in the
//! sibling modules remain for non-voice uses and as the readable reference.
//!
//! Index-based lane loops are intentional: they read/write several parallel
//! `[f32; N]` arrays in lockstep and are what the autovectoriser turns into
//! NEON. Iterator/zip forms here would obscure that, so `needless_range_loop`
//! is allowed module-wide.
#![allow(clippy::needless_range_loop)]

use crate::CHANNELS_PER_LAYER;
use crate::math::fast_sine;
use crate::oscillator::Waveform;
use crate::ota_ladder::{FilterMode, FilterSlope, OtaLadderCoeffs};

const N: usize = CHANNELS_PER_LAYER;

/// One layer's worth of noise channels — the [`crate::noise::PolyNoise`] SoA
/// generator sized to the per-layer channel count.
pub type PolyNoiseBank = crate::noise::PolyNoise<N>;

/// Branchless PolyBLEP. `dt` is floored away from zero so frozen (inactive)
/// voices can't produce NaNs; the comparison masks select the active branch.
#[inline(always)]
fn pblep(t: f32, dt: f32) -> f32 {
    let d = dt.max(1.0e-12);
    let a = t / d;
    let rise = 2.0 * a - a * a - 1.0;
    let b = (t - 1.0) / d;
    let fall = b * b + 2.0 * b + 1.0;
    let m_rise = (t < d) as u32 as f32;
    let m_fall = (t > 1.0 - d) as u32 as f32;
    rise * m_rise + fall * m_fall
}

/// Branchless `tanh` approximation: clamp to ±2.5 (where the Padé form peaks,
/// ≈0.972) then evaluate. Monotone and bounded without the early-return
/// branches of `fast_tanh`, so it vectorises.
#[inline(always)]
fn tanh_c(x: f32) -> f32 {
    let x = x.clamp(-2.5, 2.5);
    let x2 = x * x;
    let x4 = x2 * x2;
    let x6 = x4 * x2;
    x * (10395.0 + 1260.0 * x2 + 21.0 * x4) / (10395.0 + 4725.0 * x2 + 210.0 * x4 + 4.0 * x6)
}

/// Naive (pre-BLEP) oscillator value — the raw, discontinuous waveform. Used to
/// size the value jump across a hard-sync reset; the polyBLEP residual then
/// band-limits that jump. The slave's *own* wrap BLEP lives in [`osc_sample`],
/// so the sync residual must see the bare jump, not a doubly-corrected one.
#[inline(always)]
fn naive_osc(wave: Waveform, p: f32, pw: f32) -> f32 {
    match wave {
        Waveform::Sine => fast_sine(p),
        Waveform::Triangle => 1.0 - 4.0 * (p - 0.5).abs(),
        Waveform::Saw => 2.0 * p - 1.0,
        Waveform::Pulse => 1.0 - 2.0 * (p >= pw) as u32 as f32,
    }
}

/// One oscillator sample from a phase, matching the per-waveform arithmetic of
/// [`PolyOscillator::process`] exactly. Used by the coupled `process_pair_*`
/// paths, where osc1 and osc2 can carry *different* waveforms within one lane
/// loop, so the global waveform `match` can't be hoisted out per oscillator the
/// way the independent fast path does it. `wave` is loop-invariant, so the
/// branch predicts perfectly even though it sits inside the loop.
#[inline(always)]
fn osc_sample(wave: Waveform, p: f32, pw: f32, dt: f32) -> f32 {
    match wave {
        Waveform::Sine => fast_sine(p),
        Waveform::Triangle => 1.0 - 4.0 * (p - 0.5).abs(),
        Waveform::Saw => (2.0 * p - 1.0) - pblep(p, dt),
        Waveform::Pulse => {
            let naive = 1.0 - 2.0 * (p >= pw) as u32 as f32; // +1 below pw, -1 above
            let pf = {
                let x = p - pw + 1.0;
                x - x.floor()
            };
            naive + pblep(p, dt) - pblep(pf, dt)
        }
    }
}

/// Per-waveform oscillator arithmetic as a zero-sized type, so the coupled
/// `process_sync` / `process_pm` kernels can be **monomorphised** per waveform
/// (one instance per master×slave pair) instead of branching on a `Waveform`
/// enum *inside* the lane loop. The runtime `match` then sits once outside the
/// loop, leaving each lane body branch-free so it autovectorises like the
/// independent fast path ([`PolyOscillator::process`]). `naive`/`sample` mirror
/// [`naive_osc`]/[`osc_sample`] arithmetic exactly (the combined `process_pair`
/// oracle still uses those, so the differential test pins them equal).
trait WaveKind {
    /// Raw, pre-BLEP value (sizes a sync reset's jump).
    fn naive(p: f32, pw: f32) -> f32;
    /// Band-limited value (own-wrap polyBLEP applied).
    fn sample(p: f32, pw: f32, dt: f32) -> f32;
}

struct WSine;
struct WTriangle;
struct WSaw;
struct WPulse;

impl WaveKind for WSine {
    #[inline(always)]
    fn naive(p: f32, _pw: f32) -> f32 {
        fast_sine(p)
    }
    #[inline(always)]
    fn sample(p: f32, _pw: f32, _dt: f32) -> f32 {
        fast_sine(p)
    }
}
impl WaveKind for WTriangle {
    #[inline(always)]
    fn naive(p: f32, _pw: f32) -> f32 {
        1.0 - 4.0 * (p - 0.5).abs()
    }
    #[inline(always)]
    fn sample(p: f32, _pw: f32, _dt: f32) -> f32 {
        1.0 - 4.0 * (p - 0.5).abs()
    }
}
impl WaveKind for WSaw {
    #[inline(always)]
    fn naive(p: f32, _pw: f32) -> f32 {
        2.0 * p - 1.0
    }
    #[inline(always)]
    fn sample(p: f32, _pw: f32, dt: f32) -> f32 {
        (2.0 * p - 1.0) - pblep(p, dt)
    }
}
impl WaveKind for WPulse {
    #[inline(always)]
    fn naive(p: f32, pw: f32) -> f32 {
        1.0 - 2.0 * (p >= pw) as u32 as f32
    }
    #[inline(always)]
    fn sample(p: f32, pw: f32, dt: f32) -> f32 {
        let naive = 1.0 - 2.0 * (p >= pw) as u32 as f32; // +1 below pw, -1 above
        let pf = {
            let x = p - pw + 1.0;
            x - x.floor()
        };
        naive + pblep(p, dt) - pblep(pf, dt)
    }
}

/// Resolve a runtime [`Waveform`] to its [`WaveKind`] marker type, binding it to
/// `$ty` for the block `$body`. Used once outside the lane loop to dispatch the
/// monomorphised kernels; nest two calls for a master×slave pair.
macro_rules! with_wave {
    ($wave:expr, $ty:ident => $body:expr) => {
        match $wave {
            Waveform::Sine => {
                type $ty = WSine;
                $body
            }
            Waveform::Triangle => {
                type $ty = WTriangle;
                $body
            }
            Waveform::Saw => {
                type $ty = WSaw;
                $body
            }
            Waveform::Pulse => {
                type $ty = WPulse;
                $body
            }
        }
    };
}

// ── PolyOscillator ────────────────────────────────────────────────────────

/// 16-voice oscillator. Phase + increment per voice; pulse width per voice
/// (PWM modulation differs per voice).
///
/// `sync_resid` / `sync_pending` carry hard-sync polyBLEP state across samples:
/// when [`process_pair`](Self::process_pair) resets this oscillator (it is the
/// slave on osc1, with osc2 as master) sub-sample on sample *n*, the
/// discontinuity falls between samples *n* and *n+1*, so the band-limited
/// post-reset value is emitted on *n+1*. Unused (always 0) on the fast
/// [`process`](Self::process) path and on osc2 (which is never the slave).
#[derive(Clone)]
pub struct PolyOscillator {
    pub phase: [f32; N],
    pub inc: [f32; N],
    /// Residual to add to the next sample's output for a deferred sync reset.
    sync_resid: [f32; N],
    /// 1.0 on the sample following a sync reset (emit the bare post value, not
    /// the `osc_sample` free value), else 0.0.
    sync_pending: [f32; N],
    /// Sub-osc flipflop (0.0 / 1.0), toggled by the kernel that advances the
    /// phase keying it: own wrap on the independent / PM paths, master wrap on
    /// the sync path. Stored as `f32` so the lane loop stays branchless and
    /// vectorises (matches `sync_pending`). Read by [`poly_sub_square`].
    pub sub_flipflop: [f32; N],
}

impl Default for PolyOscillator {
    fn default() -> Self {
        Self::new()
    }
}

impl PolyOscillator {
    pub fn new() -> Self {
        Self {
            phase: [0.0; N],
            inc: [0.0; N],
            sync_resid: [0.0; N],
            sync_pending: [0.0; N],
            sub_flipflop: [0.0; N],
        }
    }

    #[inline]
    pub fn reset(&mut self, v: usize) {
        self.sync_resid[v] = 0.0;
        self.sync_pending[v] = 0.0;
        self.sub_flipflop[v] = 0.0;
        self.phase[v] = 0.0;
    }

    /// Produce one sample per voice into `out`, advancing all phases. `wave` is
    /// global; `pw` is per-voice pulse width. Toggles [`sub_flipflop`] on each
    /// own-wrap (drives the sub-osc on Off / Ring; unused for osc2, harmless).
    #[inline]
    pub fn process(&mut self, wave: Waveform, pw: &[f32; N], out: &mut [f32; N]) {
        match wave {
            Waveform::Sine => {
                for v in 0..N {
                    let p = self.phase[v];
                    out[v] = fast_sine(p);
                    let np = p + self.inc[v];
                    let wrapped = (np >= 1.0) as u32 as f32;
                    self.phase[v] = np - wrapped;
                    self.sub_flipflop[v] += wrapped - 2.0 * self.sub_flipflop[v] * wrapped;
                }
            }
            Waveform::Triangle => {
                for v in 0..N {
                    let p = self.phase[v];
                    out[v] = 1.0 - 4.0 * (p - 0.5).abs();
                    let np = p + self.inc[v];
                    let wrapped = (np >= 1.0) as u32 as f32;
                    self.phase[v] = np - wrapped;
                    self.sub_flipflop[v] += wrapped - 2.0 * self.sub_flipflop[v] * wrapped;
                }
            }
            Waveform::Saw => {
                for v in 0..N {
                    let p = self.phase[v];
                    let dt = self.inc[v];
                    out[v] = (2.0 * p - 1.0) - pblep(p, dt);
                    let np = p + dt;
                    let wrapped = (np >= 1.0) as u32 as f32;
                    self.phase[v] = np - wrapped;
                    self.sub_flipflop[v] += wrapped - 2.0 * self.sub_flipflop[v] * wrapped;
                }
            }
            Waveform::Pulse => {
                for v in 0..N {
                    let p = self.phase[v];
                    let dt = self.inc[v];
                    let w = pw[v];
                    let naive = 1.0 - 2.0 * (p >= w) as u32 as f32; // +1 below w, -1 above
                    let pf = {
                        let x = p - w + 1.0;
                        x - x.floor()
                    };
                    out[v] = naive + pblep(p, dt) - pblep(pf, dt);
                    let np = p + dt;
                    let wrapped = (np >= 1.0) as u32 as f32;
                    self.phase[v] = np - wrapped;
                    self.sub_flipflop[v] += wrapped - 2.0 * self.sub_flipflop[v] * wrapped;
                }
            }
        }
    }

    /// Coupled carrier(self=osc1)←modulator(osc2) path carrying **hard sync** and
    /// **through-zero phase modulation** (JP-8 VCO-2 sync + Cross Mod; ADR 0004
    /// §7). osc1 is always the audible carrier (modulated thing); osc2 is always
    /// the silent modulator (driving signal).
    ///
    /// - **Phase mod** (`pm_index` = phase-deviation index, cycles): osc2's
    ///   current output offsets osc1's **read phase** only — `o1 =
    ///   osc_sample(wave1, frac(phase_s + pm_index·o2), …)` — while osc1's phase
    ///   accumulator advances at its **unmodulated base increment**. The read
    ///   uses a **two-sided wrap** (`x − x.floor()`) so the pointer can run
    ///   backward through zero (through-zero PM); the carrier accumulator keeps
    ///   its one-sided wrap. PM ≡ FM spectrally for these timbres but with no
    ///   pitch drift and a constant `dt` (keeps polyBLEP valid and the master
    ///   `dt` = base increment). At `pm_index == 0`, `read == phase_s` exactly,
    ///   so the output is untouched.
    /// - **Sync** (`sync`): when osc2's (master) phase wraps **sub-sample** at
    ///   fraction `frac ∈ (0,1]` into the sample, osc1 (slave) resets to
    ///   `(1−frac)·inc` (the remainder of the current sample) instead of a hard
    ///   0, and osc1's value jump across that reset is band-limited with a
    ///   polyBLEP residual. This is the sub-sample path ported from
    ///   `patches-dsp` — the reset lands at the exact fractional crossing and
    ///   the edge is BLEP-softened, cutting the aliasing the sample-accurate
    ///   reset sprayed.
    ///
    /// Sync and PM are mutually exclusive at the engine (the `CrossModType`
    /// selector picks one), so the render path dispatches to the specialised
    /// [`process_sync`](Self::process_sync) / [`process_pm`](Self::process_pm)
    /// kernels (each sheds the other's work); this combined form is kept as the
    /// readable reference and the differential-test oracle. It handles both at
    /// once and stays finite.
    ///
    /// Convention: `self` is **osc1** — always the audible carrier (PM target
    /// in PM mode, sync slave in sync mode). The `other` argument is **osc2**
    /// — always the modulator (PM source in PM mode, sync master in sync mode).
    /// osc2 is evaluated first because it is the PM source feeding osc1's read
    /// offset and the master whose wrap drives osc1's sub-sample reset. This is
    /// the slow path, taken only when `sync` is on **or** `pm_index != 0`; plain
    /// patches keep the vectorised [`process`](Self::process) fast path. The
    /// reset and residual are mask-selected (not branched) so the lane loop still
    /// vectorises. High-index PM is still alias-prone; v1 leans on the engine's
    /// oversampling (and a sine-carrier bias) for that.
    #[inline]
    #[allow(clippy::too_many_arguments)] // two waves + two pw/out arrays is the coupled shape
    pub fn process_pair(
        &mut self,
        other: &mut PolyOscillator,
        sync: bool,
        pm_index: f32,
        wave1: Waveform,
        wave2: Waveform,
        pw1: &[f32; N],
        pw2: &[f32; N],
        o1: &mut [f32; N],
        o2: &mut [f32; N],
    ) {
        let sync_f = sync as u32 as f32;
        for v in 0..N {
            let dt_m = other.inc[v];
            let p_m = other.phase[v];
            let dt_s = self.inc[v];
            let p_s = self.phase[v];

            // osc2 (master / PM source) first — its free-running value is osc2's
            // output AND the PM source for osc1's read offset below.
            let s_m = osc_sample(wave2, p_m, pw2[v], dt_m);
            o2[v] = s_m;

            // osc1 (slave / carrier): through-zero phase modulation on the read,
            // plus deferred sub-sample sync reset on the bare-value path.
            // Carrier advances at its base increment (also the polyBLEP `dt` and
            // the sub-sample maths); osc2 offsets only the read phase. The summed
            // read wraps two-sided so it can run backward through zero, while the
            // accumulator below keeps its one-sided wrap. `pm_index == 0` leaves
            // `read == phase_s`, so the fast path is reproduced bit-for-bit.
            // On the sample *after* a sync reset (`sync_pending`), osc1 sits at
            // the sub-sample reset phase, so emit the **bare** waveform value plus
            // the deferred polyBLEP residual rather than `osc_sample`'s free
            // value (whose own-wrap BLEP assumes a 1→0 wrap of fixed height, not
            // this reset).
            let pend = self.sync_pending[v];
            let read = {
                let x = p_s + pm_index * s_m;
                x - x.floor()
            };
            let free_val = osc_sample(wave1, read, pw1[v], dt_s);
            let bare_val = naive_osc(wave1, p_s, pw1[v]) + self.sync_resid[v];
            let s_s = free_val * (1.0 - pend) + bare_val * pend;
            o1[v] = s_s;

            // Advance the master (osc2), capturing the wrap and its sub-sample
            // fraction: when `np_m ≥ 1`, the wrap fell `frac ∈ (0,1]` into this
            // sample, `frac = 1 − (np_m−1)/dt_m`. The `.max` guard keeps `frac`
            // finite on frozen lanes (`dt_m = 0`, which never wrap); the reset
            // mask drops it there regardless.
            let np_m = p_m + dt_m;
            let wrapped = (np_m >= 1.0) as u32 as f32;
            other.phase[v] = np_m - wrapped;
            let frac = (1.0 - (np_m - 1.0) / dt_m.max(1.0e-12)).clamp(f32::MIN_POSITIVE, 1.0);

            // On a synced master wrap, reset the slave (osc1, `self`) sub-sample
            // to `(1−frac)·dt_s` (remainder of the current sample). The master
            // wrapped *inside* this sample, so the discontinuity falls between
            // this sample and the next: defer the band-limited post value to the
            // next sample via `sync_pending` / `sync_resid`. `delta = pre − post`
            // is the bare waveform jump across the reset — `pre` the slave value
            // at the crossing instant (`p_s + frac·dt_s`), `post` the value at
            // the reset phase. Mask-selected by `wrapped · sync`, so cross-mod-
            // only patches leave the slave free and the fast path stays bit-
            // identical.
            let reset = wrapped * sync_f;
            let post_phase = (1.0 - frac) * dt_s;
            let pre_raw = p_s + frac * dt_s;
            let pre_phase = pre_raw - (pre_raw >= 1.0) as u32 as f32;
            let delta = naive_osc(wave1, pre_phase, pw1[v]) - naive_osc(wave1, post_phase, pw1[v]);
            self.sync_resid[v] = -pblep(post_phase, dt_s) * 0.5 * delta;
            self.sync_pending[v] = reset;
            // Before-side polyBLEP on the current sample (the step falls `frac`
            // into it; phase `1 − frac·dt` sits in pblep's falling region).
            let before_phase = 1.0 - frac * dt_s;
            o1[v] -= pblep(before_phase, dt_s) * 0.5 * delta * reset;

            // Slave phase for the next sample: free advance, or the reset phase
            // (un-advanced — the next sample reads it to emit the deferred post
            // value, then advances normally).
            let np_s = p_s + dt_s;
            let free_phase = np_s - (np_s >= 1.0) as u32 as f32;
            self.phase[v] = free_phase * (1.0 - reset) + post_phase * reset;
        }
    }

    /// Sync-only specialisation of [`process_pair`](Self::process_pair) (the
    /// engine picks Sync **or** PM, never both, so PM is statically absent here).
    /// Identical to the combined kernel with `pm_index == 0`: the carrier read
    /// phase is just its accumulator phase (no PM offset, no two-sided wrap), and
    /// `reset == wrapped` since sync is always on. Drops the dead `pm_index · s_m`
    /// term and its `floor` per voice per sample; the band-limited sub-sample sync
    /// machinery is all live and kept verbatim. Profiled as the dominant hot path
    /// for sync patches (`busy_profile`), so it sheds exactly the PM-only work.
    ///
    /// Convention (see [`process_pair`](Self::process_pair)): `self` is osc1 =
    /// slave/carrier (audible); `other` is osc2 = master (drives reset).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn process_sync(
        &mut self,
        other: &mut PolyOscillator,
        wave1: Waveform,
        wave2: Waveform,
        pw1: &[f32; N],
        pw2: &[f32; N],
        o1: &mut [f32; N],
        o2: &mut [f32; N],
    ) {
        // Resolve both waveforms to marker types once, outside the lane loop, so
        // the loop body is monomorphised and branch-free (see [`WaveKind`]).
        with_wave!(wave1, W1 => with_wave!(wave2, W2 => {
            self.process_sync_w::<W1, W2>(other, pw1, pw2, o1, o2)
        }))
    }

    /// Monomorphised sync lane loop. `W1` is the osc1 (slave/carrier) waveform,
    /// `W2` is the osc2 (master) waveform.
    #[inline(always)]
    fn process_sync_w<W1: WaveKind, W2: WaveKind>(
        &mut self,
        other: &mut PolyOscillator,
        pw1: &[f32; N],
        pw2: &[f32; N],
        o1: &mut [f32; N],
        o2: &mut [f32; N],
    ) {
        for v in 0..N {
            let dt_m = other.inc[v];
            let p_m = other.phase[v];
            let dt_s = self.inc[v];
            let p_s = self.phase[v];

            // osc1 (slave/carrier): free value, or the deferred bare post-reset
            // value on the sample after a sub-sample sync reset (`sync_pending`).
            let pend = self.sync_pending[v];
            let free_val = W1::sample(p_s, pw1[v], dt_s);
            let bare_val = W1::naive(p_s, pw1[v]) + self.sync_resid[v];
            let s_s = free_val * (1.0 - pend) + bare_val * pend;
            o1[v] = s_s;

            // osc2 (master): no PM, so the read phase is the accumulator phase.
            o2[v] = W2::sample(p_m, pw2[v], dt_m);

            // Advance the master, capturing the wrap and its sub-sample fraction.
            let np_m = p_m + dt_m;
            let wrapped = (np_m >= 1.0) as u32 as f32;
            other.phase[v] = np_m - wrapped;
            let frac = (1.0 - (np_m - 1.0) / dt_m.max(1.0e-12)).clamp(f32::MIN_POSITIVE, 1.0);
            // Sub flipflop is keyed to the master wrap under sync (the audible
            // period is osc2's, so the sub sits an octave below that).
            self.sub_flipflop[v] += wrapped - 2.0 * self.sub_flipflop[v] * wrapped;

            // Sync always on here: the reset mask is the bare master wrap.
            let reset = wrapped;
            let post_phase = (1.0 - frac) * dt_s;
            let pre_raw = p_s + frac * dt_s;
            let pre_phase = pre_raw - (pre_raw >= 1.0) as u32 as f32;
            let delta = W1::naive(pre_phase, pw1[v]) - W1::naive(post_phase, pw1[v]);
            self.sync_resid[v] = -pblep(post_phase, dt_s) * 0.5 * delta;
            self.sync_pending[v] = reset;
            let before_phase = 1.0 - frac * dt_s;
            o1[v] -= pblep(before_phase, dt_s) * 0.5 * delta * reset;

            let np_s = p_s + dt_s;
            let free_phase = np_s - (np_s >= 1.0) as u32 as f32;
            self.phase[v] = free_phase * (1.0 - reset) + post_phase * reset;
        }
    }

    /// Phase-mod-only specialisation of [`process_pair`](Self::process_pair).
    /// With sync statically off the modulator is a plain free-running oscillator
    /// (its output is the PM source), so the entire sub-sample reset apparatus —
    /// the `frac` solve, the `delta` (two extra [`naive_osc`]), the two [`pblep`]
    /// residuals — collapses to nothing and is dropped. That dead-but-computed
    /// work was ~half the combined kernel's cost (see `busy_profile`), so PM
    /// patches roughly halve their oscillator time.
    ///
    /// Bit-identical to the combined kernel in steady PM state: there `sync == 0`
    /// forces `reset == 0`, so `sync_pending` reads 0 and the carrier emits its
    /// free value. The one stored `sync_pending = 0` (on the carrier — `self`
    /// is the slave when sync is engaged) keeps a later switch back to sync
    /// clean (a fresh note resets the rest via [`reset`](Self::reset)).
    ///
    /// Convention (see [`process_pair`](Self::process_pair)): `self` is osc1 =
    /// carrier (PM target); `other` is osc2 = modulator (PM source).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn process_pm(
        &mut self,
        other: &mut PolyOscillator,
        pm_index: f32,
        wave1: Waveform,
        wave2: Waveform,
        pw1: &[f32; N],
        pw2: &[f32; N],
        o1: &mut [f32; N],
        o2: &mut [f32; N],
    ) {
        with_wave!(wave1, W1 => with_wave!(wave2, W2 => {
            self.process_pm_w::<W1, W2>(other, pm_index, pw1, pw2, o1, o2)
        }))
    }

    /// Monomorphised PM lane loop. `W1` is the osc1 (carrier) waveform, `W2`
    /// is the osc2 (modulator) waveform.
    #[inline(always)]
    fn process_pm_w<W1: WaveKind, W2: WaveKind>(
        &mut self,
        other: &mut PolyOscillator,
        pm_index: f32,
        pw1: &[f32; N],
        pw2: &[f32; N],
        o1: &mut [f32; N],
        o2: &mut [f32; N],
    ) {
        for v in 0..N {
            let dt_m = other.inc[v];
            let p_m = other.phase[v];

            // osc2 (modulator): free-running PM source.
            let s_m = W2::sample(p_m, pw2[v], dt_m);
            o2[v] = s_m;
            let np_m = p_m + dt_m;
            let wrapped_m = (np_m >= 1.0) as u32 as f32;
            other.phase[v] = np_m - wrapped_m;
            other.sub_flipflop[v] += wrapped_m - 2.0 * other.sub_flipflop[v] * wrapped_m;

            // osc1 (carrier): through-zero phase mod. The accumulator advances at
            // the base increment; the modulator offsets only the read, which wraps
            // two-sided so it can run backward through zero. Clear any
            // `sync_pending` left on the carrier by a prior sync block so a later
            // switch back to sync starts clean. The sub-osc flipflop tracks the
            // carrier's accumulator wrap (PM doesn't modulate it), so sub
            // frequency stays at `osc1_accumulator / 2` regardless of `pm_index`.
            let p_s = self.phase[v];
            let inc_c = self.inc[v];
            let read = {
                let x = p_s + pm_index * s_m;
                x - x.floor()
            };
            o1[v] = W1::sample(read, pw1[v], inc_c);
            self.sync_pending[v] = 0.0;
            let np_s = p_s + inc_c;
            let wrapped_s = (np_s >= 1.0) as u32 as f32;
            self.phase[v] = np_s - wrapped_s;
            self.sub_flipflop[v] += wrapped_s - 2.0 * self.sub_flipflop[v] * wrapped_s;
        }
    }
}

// ── Sub-osc (Juno-style square one octave below the source) ────────────────

/// Band-limited square at half the source frequency, phase-locked to the
/// source via a flipflop toggled on each source wrap. `phase`/`inc` are the
/// source oscillator's (osc1's accumulator on Off/Ring/PM, osc2's master on
/// Sync); `flip` is the per-voice flipflop the source's kernel toggles. The
/// sub phase is `source_phase/2 + flip·½` and the increment is `source_inc/2`,
/// so two source wraps make one full sub cycle (sub period = 2× source).
/// PolyBLEP is applied on both the sub's wrap and the half-cycle duty edge.
/// Branchless, vectorising.
#[inline]
pub fn poly_sub_square(
    phase: &[f32; N],
    inc: &[f32; N],
    flip: &[f32; N],
    out: &mut [f32; N],
) {
    for v in 0..N {
        let sp = phase[v] * 0.5 + flip[v] * 0.5;
        let sdt = inc[v] * 0.5;
        let naive = 1.0 - 2.0 * (sp >= 0.5) as u32 as f32;
        let pf = {
            let x = sp - 0.5 + 1.0;
            x - x.floor()
        };
        out[v] = naive + pblep(sp, sdt) - pblep(pf, sdt);
    }
}

// ── Ring modulator (Parker diode-bridge, DAFx-11) ──────────────────────────

/// Half-wave diode with the Parker 5th-order I–V fit + tanh soft-clip, gain-
/// compensated. Branchless: the `x > 0` clamp is a multiply mask (the reference
/// early-returns 0 for non-positive inputs) so the lane loop vectorises. `gain` =
/// `10^(drive_dB/20)`; low gain ≈ near-ideal multiply, high gain = harmonic
/// colouring. Ported from `patches-modules::modulators::ring_mod::diode`.
#[inline(always)]
fn ring_diode(x: f32, gain: f32) -> f32 {
    let i = x * gain;
    let i2 = i * i;
    let i3 = i2 * i;
    let i4 = i3 * i;
    let i5 = i4 * i;
    let v = i5 * (-0.0025) + i4 * 0.0451 + i3 * (-0.3043) + i2 * 0.9589 + i * (-0.3828) + 0.0061;
    let mask = (x > 0.0) as u32 as f32;
    tanh_c(v) / gain * mask
}

/// Push-pull diode pair (full-wave): processes both polarities of `x`. Even in
/// `x`, so `diode_block(c) == diode_block(-c)` — the property that silences a
/// zero-signal input.
#[inline(always)]
fn ring_diode_block(x: f32, gain: f32) -> f32 {
    ring_diode(x, gain) + ring_diode(-x, gain)
}

/// Parker diode-bridge ring modulator over a layer's voices (SoA, stateless):
/// `out[v] = diode_block(o1 + ½·o2) − diode_block(o1 − ½·o2)`. Zero on either
/// input ⇒ ~silence (a zero carrier makes the two blocks equal; the block is
/// even, so a zero signal does too). `gain` is the diode operating point. The
/// caller mixes the result into the osc1 slot when `CrossModType::Ring` is on.
#[inline]
pub fn poly_ring_mod(o1: &[f32; N], o2: &[f32; N], gain: f32, out: &mut [f32; N]) {
    for v in 0..N {
        let c = o2[v] * 0.5;
        out[v] = ring_diode_block(o1[v] + c, gain) - ring_diode_block(o1[v] - c, gain);
    }
}

// ── PolyOtaLadder ─────────────────────────────────────────────────────────────

/// 16-voice OTA-C ladder lowpass (R3109/IR3109-style, Juno-flavoured). Poly
/// sibling of [`crate::ota_ladder::OtaLadderKernel`].
///
/// Coefficients are *interpolated per sample* across each control block: the
/// engine samples the modulators once per block, calls [`set_coeffs`](Self::set_coeffs)
/// with the block target then [`prepare_ramp`](Self::prepare_ramp), and
/// [`process`](Self::process) linearly ramps `(g, k, drive)` from the previous
/// block's values toward it — turning block-stepped cutoff into a smooth
/// piecewise-linear trajectory (no zipper/staircase).
///
/// The nonlinearity is per-stage `tanh` and there is **no** `scale` term — the
/// OTA design does not thin the bass under resonance. `mode` (LP/BP/HP/Notch,
/// see [`FilterMode`]) is a *layer-wide* parameter, hoisted out of the lane
/// loop; the feedback path is always the 4th stage so resonance is identical in
/// every mode.
#[derive(Clone)]
pub struct PolyOtaLadder {
    // Current (interpolated) coefficients, advanced each sample.
    g: [f32; N],
    k: [f32; N],
    drive: [f32; N],
    // Per-sample increments toward the target (set by `prepare_ramp`).
    dg: [f32; N],
    dk: [f32; N],
    dd: [f32; N],
    // Block target coefficients (set by `set_coeffs`).
    tg: [f32; N],
    tk: [f32; N],
    td: [f32; N],
    s0: [f32; N],
    s1: [f32; N],
    s2: [f32; N],
    s3: [f32; N],
    y4: [f32; N],
    mode: FilterMode,
    slope: FilterSlope,
}

impl Default for PolyOtaLadder {
    fn default() -> Self {
        Self::new()
    }
}

impl PolyOtaLadder {
    pub fn new() -> Self {
        Self {
            g: [0.5; N],
            k: [0.0; N],
            drive: [1.0; N],
            dg: [0.0; N],
            dk: [0.0; N],
            dd: [0.0; N],
            tg: [0.5; N],
            tk: [0.0; N],
            td: [1.0; N],
            s0: [0.0; N],
            s1: [0.0; N],
            s2: [0.0; N],
            s3: [0.0; N],
            y4: [0.0; N],
            mode: FilterMode::Lp,
            slope: FilterSlope::Pole4,
        }
    }

    pub fn reset(&mut self) {
        self.s0 = [0.0; N];
        self.s1 = [0.0; N];
        self.s2 = [0.0; N];
        self.s3 = [0.0; N];
        self.y4 = [0.0; N];
    }

    /// Set the filter response + slope (layer-wide). Feedback path is unchanged.
    #[inline]
    pub fn set_response(&mut self, mode: FilterMode, slope: FilterSlope) {
        self.mode = mode;
        self.slope = slope;
    }

    pub fn mode(&self) -> FilterMode {
        self.mode
    }

    pub fn slope(&self) -> FilterSlope {
        self.slope
    }

    /// Set this block's *target* coefficients for voice `v`.
    #[inline]
    pub fn set_coeffs(&mut self, v: usize, c: OtaLadderCoeffs) {
        self.tg[v] = c.g;
        self.tk[v] = c.k;
        self.td[v] = c.drive;
    }

    /// Compute per-sample increments so the current coefficients reach their
    /// targets after exactly `steps` [`process`] calls. `steps <= 1` snaps.
    #[inline]
    pub fn prepare_ramp(&mut self, steps: usize) {
        if steps <= 1 {
            self.snap_coeffs();
            return;
        }
        let inv = 1.0 / steps as f32;
        for v in 0..N {
            self.dg[v] = (self.tg[v] - self.g[v]) * inv;
            self.dk[v] = (self.tk[v] - self.k[v]) * inv;
            self.dd[v] = (self.td[v] - self.drive[v]) * inv;
        }
    }

    /// Jump current coefficients to the targets with no ramp.
    #[inline]
    pub fn snap_coeffs(&mut self) {
        self.g = self.tg;
        self.k = self.tk;
        self.drive = self.td;
        self.dg = [0.0; N];
        self.dk = [0.0; N];
        self.dd = [0.0; N];
    }

    /// One sample per voice: `out[v] = ota_ladder(x[v])`, mixed for the mode/slope.
    #[inline]
    pub fn process(&mut self, x: &[f32; N], out: &mut [f32; N]) {
        let mode = self.mode;
        let slope = self.slope;
        for v in 0..N {
            let g = self.g[v];
            let fed = self.drive[v] * x[v] - self.k[v] * self.y4[v];

            let u0 = tanh_c(fed);
            let a0 = (u0 - self.s0[v]) * g;
            let y0 = a0 + self.s0[v];
            self.s0[v] = y0 + a0;

            let u1 = tanh_c(y0);
            let a1 = (u1 - self.s1[v]) * g;
            let y1 = a1 + self.s1[v];
            self.s1[v] = y1 + a1;

            let u2 = tanh_c(y1);
            let a2 = (u2 - self.s2[v]) * g;
            let y2 = a2 + self.s2[v];
            self.s2[v] = y2 + a2;

            let u3 = tanh_c(y2);
            let a3 = (u3 - self.s3[v]) * g;
            let y3 = a3 + self.s3[v];
            self.s3[v] = y3 + a3;

            self.y4[v] = y3;
            out[v] = mode.mix(slope, fed, [y0, y1, y2, y3]);

            // Advance interpolated coefficients toward the block target.
            self.g[v] += self.dg[v];
            self.k[v] += self.dk[v];
            self.drive[v] += self.dd[v];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oscillator::Oscillator;

    #[test]
    fn poly_saw_matches_scalar_within_tolerance() {
        // Lane 0 of the poly oscillator should track a scalar saw closely
        // (same polyblep maths, branchless form).
        let inc = 220.0 / 48_000.0;
        let mut poly = PolyOscillator::new();
        poly.inc[0] = inc;
        let mut scalar = Oscillator::new();
        scalar.set_increment(inc);

        let pw = [0.5; N];
        let mut out = [0.0; N];
        let mut max_diff = 0.0f32;
        for _ in 0..4800 {
            poly.process(Waveform::Saw, &pw, &mut out);
            let s = scalar.next(Waveform::Saw);
            max_diff = max_diff.max((out[0] - s).abs());
        }
        assert!(max_diff < 1e-5, "poly vs scalar saw diff {max_diff}");
    }

    #[test]
    fn poly_osc_all_lanes_bounded() {
        let mut poly = PolyOscillator::new();
        for v in 0..N {
            poly.inc[v] = (50.0 + v as f32 * 40.0) / 48_000.0;
        }
        let pw = [0.5; N];
        let mut out = [0.0; N];
        for wave in Waveform::ALL {
            for _ in 0..4800 {
                poly.process(wave, &pw, &mut out);
                assert!(
                    out.iter().all(|s| s.is_finite() && s.abs() <= 2.0),
                    "{wave:?}"
                );
            }
        }
    }

    #[test]
    fn frozen_voice_produces_no_nan() {
        // inc = 0 (inactive voice): pblep must not divide by zero.
        let mut poly = PolyOscillator::new();
        let pw = [0.5; N];
        let mut out = [0.0; N];
        for _ in 0..100 {
            poly.process(Waveform::Pulse, &pw, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }

    /// The specialised `process_sync` / `process_pm` kernels must match the
    /// combined `process_pair` oracle: sync ≡ `process_pair(sync=true, pm=0)`,
    /// PM ≡ `process_pair(sync=false, pm=index)`. Drives detuned master/slave
    /// across every osc1×osc2 waveform pair and a moving pulse width so the BLEP
    /// and reset branches all fire, and checks both osc outputs every sample.
    #[test]
    fn specialised_cross_mod_kernels_match_combined() {
        fn pair() -> (PolyOscillator, PolyOscillator) {
            let mut m = PolyOscillator::new();
            let mut s = PolyOscillator::new();
            for v in 0..N {
                // Distinct, detuned incs per lane; slave runs higher (sync/PM
                // motion). Lane 0 left at inc 0 to exercise the frozen guard.
                if v > 0 {
                    m.inc[v] = (60.0 + v as f32 * 30.0) / 48_000.0;
                    s.inc[v] = (90.0 + v as f32 * 55.0) / 48_000.0;
                }
            }
            (m, s)
        }

        for wave1 in Waveform::ALL {
            for wave2 in Waveform::ALL {
                // Sync: combined(sync=true, pm=0) vs process_sync.
                let (mut ma, mut sa) = pair();
                let (mut mb, mut sb) = pair();
                // PM: combined(sync=false, pm=index) vs process_pm.
                let (mut mc, mut sc) = pair();
                let (mut md, mut sd) = pair();
                let (mut oa1, mut oa2) = ([0.0; N], [0.0; N]);
                let (mut ob1, mut ob2) = ([0.0; N], [0.0; N]);
                let (mut oc1, mut oc2) = ([0.0; N], [0.0; N]);
                let (mut od1, mut od2) = ([0.0; N], [0.0; N]);

                for i in 0..2000 {
                    // Sweep pulse width so the pulse BLEP edges move.
                    let w = 0.3 + 0.4 * (i as f32 * 0.01).sin().abs();
                    let pw1 = [w; N];
                    let pw2 = [0.5; N];

                    ma.process_pair(
                        &mut sa, true, 0.0, wave1, wave2, &pw1, &pw2, &mut oa1, &mut oa2,
                    );
                    mb.process_sync(&mut sb, wave1, wave2, &pw1, &pw2, &mut ob1, &mut ob2);

                    mc.process_pair(
                        &mut sc, false, 0.7, wave1, wave2, &pw1, &pw2, &mut oc1, &mut oc2,
                    );
                    md.process_pm(&mut sd, 0.7, wave1, wave2, &pw1, &pw2, &mut od1, &mut od2);

                    for v in 0..N {
                        assert!(
                            (oa1[v] - ob1[v]).abs() < 1e-6 && (oa2[v] - ob2[v]).abs() < 1e-6,
                            "sync mismatch {wave1:?}/{wave2:?} lane {v} i {i}: \
                             o1 {} vs {}, o2 {} vs {}",
                            oa1[v],
                            ob1[v],
                            oa2[v],
                            ob2[v]
                        );
                        assert!(
                            (oc1[v] - od1[v]).abs() < 1e-6 && (oc2[v] - od2[v]).abs() < 1e-6,
                            "pm mismatch {wave1:?}/{wave2:?} lane {v} i {i}: \
                             o1 {} vs {}, o2 {} vs {}",
                            oc1[v],
                            od1[v],
                            oc2[v],
                            od2[v]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn poly_ladder_stable_and_lowpass() {
        let sr = 48_000.0;
        let mut lad = PolyOtaLadder::new();
        for v in 0..N {
            lad.set_coeffs(v, OtaLadderCoeffs::new(1000.0, sr, 0.5, 1.0));
        }
        lad.snap_coeffs();
        // Feed Nyquist-ish into all lanes; should be attenuated and finite.
        let mut peak = 0.0f32;
        let mut out = [0.0; N];
        for i in 0..4800 {
            let s = if i % 2 == 0 { 0.1 } else { -0.1 };
            let x = [s; N];
            lad.process(&x, &mut out);
            peak = peak.max(out[0].abs());
            assert!(out.iter().all(|y| y.is_finite()));
        }
        assert!(peak < 0.1, "hf not attenuated: {peak}");
    }

    #[test]
    fn ladder_coeffs_interpolate_across_block() {
        // prepare_ramp must land the current coefficients exactly on target
        // after `steps` process calls, ramping linearly (no jump on sample 0).
        let sr = 48_000.0;
        let mut lad = PolyOtaLadder::new();
        // Start settled at a low cutoff, then target a high one.
        for v in 0..N {
            lad.set_coeffs(v, OtaLadderCoeffs::new(200.0, sr, 0.0, 1.0));
        }
        lad.snap_coeffs();
        let g_start = lad.g[0];
        let target = OtaLadderCoeffs::new(8000.0, sr, 0.0, 1.0);
        for v in 0..N {
            lad.set_coeffs(v, target);
        }
        let steps = 32;
        lad.prepare_ramp(steps);
        let x = [0.0; N];
        let mut out = [0.0; N];
        // After one step the coefficient has moved only a fraction of the way.
        lad.process(&x, &mut out);
        let after_one = lad.g[0];
        assert!(
            after_one > g_start && after_one < target.g,
            "no mid-ramp value: start {g_start}, after1 {after_one}, target {}",
            target.g
        );
        // Remaining steps land on (≈) the target.
        for _ in 1..steps {
            lad.process(&x, &mut out);
        }
        assert!(
            (lad.g[0] - target.g).abs() < 1e-5,
            "ramp missed target: {} vs {}",
            lad.g[0],
            target.g
        );
    }

    #[test]
    fn synced_slave_locks_to_master_period() {
        // Master (osc2) at a power-of-two sample period (512, so 1/512 is exact
        // in f32 and the master wrap fraction repeats bit-exactly); slave (osc1)
        // tuned well above and not a divisor of it. With sync, the slave resets
        // at every master wrap, so its output is exactly periodic at the master's
        // period.
        let period = 512usize;
        let mut osc1 = PolyOscillator::new();
        let mut osc2 = PolyOscillator::new();
        osc1.inc[0] = 1.0 / 63.0; // slave: ~7.6× master, non-divisor
        osc2.inc[0] = 1.0 / period as f32; // master
        let pw = [0.5; N];
        let (mut o1, mut o2) = ([0.0; N], [0.0; N]);

        // Capture two full master periods after a one-period warm-up.
        let mut log = Vec::with_capacity(2 * period);
        for i in 0..(3 * period) {
            osc1.process_pair(
                &mut osc2,
                true,
                0.0,
                Waveform::Sine, // osc1 (slave/carrier — audible)
                Waveform::Saw,  // osc2 (master)
                &pw,
                &pw,
                &mut o1,
                &mut o2,
            );
            if i >= period {
                log.push(o1[0]); // slave output is the audible sync sound
            }
        }
        // Slave output repeats with the master's period (sync lock).
        let mut max_diff = 0.0f32;
        for i in 0..period {
            max_diff = max_diff.max((log[i] - log[i + period]).abs());
        }
        assert!(
            max_diff < 1e-6,
            "slave not locked to master period: {max_diff}"
        );
    }

    /// Sub-sample polyBLEP sync sprays materially less aliasing than the old
    /// sample-accurate hard reset. Modelled on
    /// `patches-integration-tests/tests/hard_sync_aliasing.rs`: render a synced
    /// saw both ways, take the magnitude spectrum (rectangular window — the
    /// signal is periodic in the window by construction), and compare energy in
    /// the upper eighth of the spectrum, which a BLEP-smoothed synced saw keeps
    /// nearly empty while the boundary-rounded reset fills with broadband noise.
    #[test]
    fn subsample_sync_beats_sample_accurate_aliasing() {
        const SR: f32 = 48_000.0;
        const NFFT: usize = 4096;
        // 40 cycles of the master fit exactly in the window (bin-aligned), so
        // the synced output is periodic in NFFT and needs no window.
        const K: usize = 40;
        let f_master = K as f32 * SR / NFFT as f32; // 468.75 Hz
        let f_slave = f_master * 1.5; // 3:2 sync

        // Old sample-accurate path: hard reset to 0 on the master wrap, no
        // residual. Mirrors the pre-0020 `process_pair` slave handling.
        fn process_naive(
            slave: &mut PolyOscillator,
            master: &mut PolyOscillator,
            o_s: &mut [f32; N],
        ) {
            for v in 0..N {
                o_s[v] = osc_sample(Waveform::Saw, slave.phase[v], 0.5, slave.inc[v]);
                let np_m = master.phase[v] + master.inc[v];
                let wrapped = (np_m >= 1.0) as u32 as f32;
                master.phase[v] = np_m - wrapped;
                let np_s = slave.phase[v] + slave.inc[v];
                slave.phase[v] = (np_s - (np_s >= 1.0) as u32 as f32) * (1.0 - wrapped);
            }
        }

        fn render(subsample: bool, f_master: f32, f_slave: f32) -> Vec<f32> {
            // osc1 = slave/carrier (audible), osc2 = master.
            let mut slave = PolyOscillator::new();
            let mut master = PolyOscillator::new();
            slave.inc[0] = f_slave / SR;
            master.inc[0] = f_master / SR;
            let pw = [0.5; N];
            let (mut o1, mut o2) = ([0.0; N], [0.0; N]);
            // Warm up past the initial transient, then capture one window.
            for _ in 0..NFFT {
                if subsample {
                    slave.process_pair(
                        &mut master,
                        true,
                        0.0,
                        Waveform::Saw,
                        Waveform::Saw,
                        &pw,
                        &pw,
                        &mut o1,
                        &mut o2,
                    );
                } else {
                    process_naive(&mut slave, &mut master, &mut o1);
                }
            }
            let mut out = Vec::with_capacity(NFFT);
            for _ in 0..NFFT {
                if subsample {
                    slave.process_pair(
                        &mut master,
                        true,
                        0.0,
                        Waveform::Saw,
                        Waveform::Saw,
                        &pw,
                        &pw,
                        &mut o1,
                        &mut o2,
                    );
                } else {
                    process_naive(&mut slave, &mut master, &mut o1);
                }
                out.push(o1[0]); // slave output is the audible sync sound
            }
            out
        }

        // Naive DFT magnitude sum over the upper eighth (bins 3N/8..N/2).
        fn high_band_energy(x: &[f32]) -> f64 {
            let n = x.len();
            let start = 3 * n / 8;
            let end = n / 2;
            let mut total = 0.0f64;
            for k in start..end {
                let w = std::f64::consts::TAU * k as f64 / n as f64;
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, &s) in x.iter().enumerate() {
                    let ph = w * i as f64;
                    re += s as f64 * ph.cos();
                    im -= s as f64 * ph.sin();
                }
                total += (re * re + im * im).sqrt();
            }
            total
        }

        let _ = f_slave;
        // Several inharmonic ratios; the floor across them is ~1.5×, so require
        // the sample-accurate reset to spray at least 1.4× the BLEP path's
        // high-band energy (margin tuned with headroom against regressions).
        for ratio in [1.5_f32, 1.618_034, 2.5, 3.5] {
            let blep = render(true, f_master, f_master * ratio);
            let naive = render(false, f_master, f_master * ratio);
            assert!(
                blep.iter().all(|v| v.is_finite()),
                "ratio {ratio}: non-finite"
            );
            let blep_hi = high_band_energy(&blep);
            let naive_hi = high_band_energy(&naive);
            assert!(
                naive_hi > blep_hi * 1.4,
                "ratio {ratio}: expected sample-accurate aliasing ({naive_hi:.2}) to \
                 exceed sub-sample BLEP ({blep_hi:.2}) by >1.4×"
            );
        }
    }

    #[test]
    fn synced_pair_all_lanes_finite() {
        // Mixed waveforms, varied tunings, and a frozen (inc = 0) lane: the
        // coupled path must stay finite, including the masked phase reset.
        // osc1 = slave/carrier (high freq), osc2 = master (low freq).
        let mut osc1 = PolyOscillator::new();
        let mut osc2 = PolyOscillator::new();
        for v in 0..N {
            osc1.inc[v] = (300.0 + v as f32 * 90.0) / 48_000.0; // slave/carrier
            osc2.inc[v] = (40.0 + v as f32 * 30.0) / 48_000.0; // master
        }
        osc1.inc[3] = 0.0; // frozen slave lane
        osc2.inc[5] = 0.0; // frozen master lane
        let pw = [0.5; N];
        let (mut o1, mut o2) = ([0.0; N], [0.0; N]);
        for (w1, w2) in [
            (Waveform::Saw, Waveform::Pulse),
            (Waveform::Pulse, Waveform::Saw),
            (Waveform::Sine, Waveform::Triangle),
        ] {
            for _ in 0..4800 {
                // sync on + heavy cross-mod: both couplings active at once.
                osc1.process_pair(&mut osc2, true, 0.9, w1, w2, &pw, &pw, &mut o1, &mut o2);
                assert!(
                    o1.iter().chain(o2.iter()).all(|s| s.is_finite()),
                    "{w1:?}/{w2:?}"
                );
            }
        }
    }

    #[test]
    fn coupled_xmod_zero_matches_fast_path() {
        // The coupled path with sync off and PM index 0 must be bit-identical to
        // the independent fast path (`read == phase1`, no reset), so plain patches
        // selecting the fast path lose nothing.
        let mut a1 = PolyOscillator::new();
        let mut a2 = PolyOscillator::new();
        let mut b1 = PolyOscillator::new();
        let mut b2 = PolyOscillator::new();
        for v in 0..N {
            let i1 = (60.0 + v as f32 * 25.0) / 48_000.0;
            let i2 = (90.0 + v as f32 * 55.0) / 48_000.0;
            a1.inc[v] = i1;
            b1.inc[v] = i1;
            a2.inc[v] = i2;
            b2.inc[v] = i2;
        }
        a1.inc[7] = 0.0; // frozen lane
        b1.inc[7] = 0.0;
        let pw = [0.5; N];
        let (mut fo1, mut fo2) = ([0.0; N], [0.0; N]);
        let (mut co1, mut co2) = ([0.0; N], [0.0; N]);
        for _ in 0..4800 {
            // Fast path.
            a1.process(Waveform::Saw, &pw, &mut fo1);
            a2.process(Waveform::Pulse, &pw, &mut fo2);
            // Coupled path, sync off, depth 0.
            b1.process_pair(
                &mut b2,
                false,
                0.0,
                Waveform::Saw,
                Waveform::Pulse,
                &pw,
                &pw,
                &mut co1,
                &mut co2,
            );
            assert_eq!(fo1, co1, "osc1 diverged from fast path");
            assert_eq!(fo2, co2, "osc2 diverged from fast path");
        }
    }

    #[test]
    fn cross_mod_adds_spectral_content() {
        // Cross-mod of a sine carrier (f1) by a sine modulator (f2) creates
        // sidebands at f1 ± f2. Measure the magnitude at the f1+f2 bin via a
        // single-bin DFT: ≈0 at depth 0, clearly present at depth > 0.
        let sr = 48_000.0;
        let f1 = 110.0f32;
        let f2 = 270.0f32; // inharmonic ratio
        fn sideband(xmod: f32, f1: f32, f2: f32, sr: f32) -> f32 {
            let mut osc1 = PolyOscillator::new();
            let mut osc2 = PolyOscillator::new();
            osc1.inc[0] = f1 / sr;
            osc2.inc[0] = f2 / sr;
            let pw = [0.5; N];
            let (mut o1, mut o2) = ([0.0; N], [0.0; N]);
            let w = std::f32::consts::TAU * (f1 + f2) / sr;
            let (mut re, mut im) = (0.0f32, 0.0f32);
            let frames = 8192usize;
            // Hann window so the strong carrier's spectral leakage doesn't swamp
            // the sideband bin (rectangular sidelobes fall off only as 1/k).
            for n in 0..frames {
                osc1.process_pair(
                    &mut osc2,
                    false,
                    xmod,
                    Waveform::Sine,
                    Waveform::Sine,
                    &pw,
                    &pw,
                    &mut o1,
                    &mut o2,
                );
                let win =
                    0.5 * (1.0 - (std::f32::consts::TAU * n as f32 / (frames - 1) as f32).cos());
                let ph = w * n as f32;
                re += o1[0] * win * ph.cos();
                im -= o1[0] * win * ph.sin();
            }
            (re * re + im * im).sqrt() / frames as f32
        }
        let clean = sideband(0.0, f1, f2, sr);
        let modulated = sideband(0.6, f1, f2, sr);
        assert!(
            modulated > 10.0 * clean.max(1e-6),
            "cross-mod produced no sideband: clean {clean}, modulated {modulated}"
        );
    }

    /// Default ring drive gain (mirrors the engine's fixed operating point).
    fn ring_gain() -> f32 {
        10.0_f32.powf(1.0 / 20.0)
    }

    #[test]
    fn ring_mod_zero_input_silences() {
        // Zero carrier (o2) or zero signal (o1) ⇒ ~silence (mirrors patches'
        // zero_carrier_silences_output / zero_signal_silences_output).
        let g = ring_gain();
        let mut out = [1.0; N];
        // Zero carrier across all lanes.
        poly_ring_mod(&[0.7; N], &[0.0; N], g, &mut out);
        assert!(
            out.iter().all(|y| y.abs() < 1e-6),
            "zero carrier not silent"
        );
        // Zero signal across all lanes.
        poly_ring_mod(&[0.0; N], &[0.7; N], g, &mut out);
        assert!(out.iter().all(|y| y.abs() < 1e-6), "zero signal not silent");
    }

    #[test]
    fn ring_mod_nonzero_inputs_produce_output() {
        let mut out = [0.0; N];
        poly_ring_mod(&[0.5; N], &[0.5; N], ring_gain(), &mut out);
        assert!(
            out.iter().all(|y| y.abs() > 1e-4),
            "expected nonzero output"
        );
    }

    /// Naive DFT magnitude at a single bin (used by the sub-osc pitch tests —
    /// avoids pulling in a full FFT for a few hundred samples).
    fn dft_mag(x: &[f32], k: usize) -> f64 {
        let n = x.len();
        let w = std::f64::consts::TAU * k as f64 / n as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            let ph = w * i as f64;
            re += s as f64 * ph.cos();
            im -= s as f64 * ph.sin();
        }
        (re * re + im * im).sqrt()
    }

    /// Bin with the largest magnitude (excluding DC).
    fn peak_bin(x: &[f32]) -> usize {
        let n = x.len();
        let (mut best_k, mut best) = (0usize, 0.0f64);
        for k in 1..n / 2 {
            let m = dft_mag(x, k);
            if m > best {
                best = m;
                best_k = k;
            }
        }
        best_k
    }

    #[test]
    fn poly_sub_square_matches_scalar_within_tolerance() {
        // Mirror `poly_saw_matches_scalar_within_tolerance`: drive lane 0 of a
        // PolyOscillator on the saw fast path; the same flipflop-toggle and
        // half-rate comparator built scalar-side must track within 1e-5.
        for f_hz in [220.0_f32, 750.0, 1234.0] {
            let inc = f_hz / 48_000.0;
            let mut poly = PolyOscillator::new();
            poly.inc[0] = inc;
            let mut sp_ref = 0.0f32;
            let mut flip_ref = 0.0f32;
            let pw = [0.5; N];
            let mut osc_out = [0.0; N];
            let mut sub_out = [0.0; N];
            let mut max_diff = 0.0f32;
            for _ in 0..4800 {
                poly.process(Waveform::Saw, &pw, &mut osc_out);
                poly_sub_square(&poly.phase, &poly.inc, &poly.sub_flipflop, &mut sub_out);
                // Advance scalar source identically (emit-then-advance order).
                let np = sp_ref + inc;
                let wrapped = (np >= 1.0) as u32 as f32;
                sp_ref = np - wrapped;
                flip_ref += wrapped - 2.0 * flip_ref * wrapped;
                // Compute scalar sub at the post-advance state (matches kernel).
                let sp = sp_ref * 0.5 + flip_ref * 0.5;
                let sdt = inc * 0.5;
                let naive = 1.0 - 2.0 * (sp >= 0.5) as u32 as f32;
                let pf = {
                    let x = sp - 0.5 + 1.0;
                    x - x.floor()
                };
                let want = naive + pblep(sp, sdt) - pblep(pf, sdt);
                max_diff = max_diff.max((sub_out[0] - want).abs());
            }
            assert!(max_diff < 1e-5, "f={f_hz}: diff {max_diff}");
        }
    }

    #[test]
    fn sub_pitch_off_is_source_half() {
        // Off / Ring path uses `process`; sub frequency = osc1 / 2.
        const SR: f32 = 48_000.0;
        const NFFT: usize = 1024;
        const K: usize = 32; // even → K/2 integer; f_src = 1500 Hz, sub = 750 Hz
        let f_src = K as f32 * SR / NFFT as f32;
        let mut osc = PolyOscillator::new();
        osc.inc[0] = f_src / SR;
        let pw = [0.5; N];
        let mut o = [0.0; N];
        let mut sub = [0.0; N];
        for _ in 0..NFFT {
            osc.process(Waveform::Sine, &pw, &mut o);
        }
        let mut buf = Vec::with_capacity(NFFT);
        for _ in 0..NFFT {
            osc.process(Waveform::Sine, &pw, &mut o);
            poly_sub_square(&osc.phase, &osc.inc, &osc.sub_flipflop, &mut sub);
            buf.push(sub[0]);
        }
        assert_eq!(peak_bin(&buf), K / 2, "sub fundamental not at source/2");
    }

    #[test]
    fn sub_pitch_under_pm_independent_of_amount() {
        // FM path uses `process_pm`; PM offsets only the read phase, not the
        // accumulator that drives the flipflop, so sub pitch is constant
        // regardless of `pm_index`.
        const SR: f32 = 48_000.0;
        const NFFT: usize = 1024;
        const K: usize = 32;
        let f1 = K as f32 * SR / NFFT as f32;
        let f2 = f1 * 1.618_034; // inharmonic — keeps modulator out of K/2
        let pw = [0.5; N];
        for amt in [0.0_f32, 0.5, 1.5, 3.0] {
            let mut o1 = PolyOscillator::new();
            let mut o2 = PolyOscillator::new();
            o1.inc[0] = f1 / SR;
            o2.inc[0] = f2 / SR;
            let mut a = [0.0; N];
            let mut b = [0.0; N];
            let mut sub = [0.0; N];
            for _ in 0..NFFT {
                o1.process_pm(
                    &mut o2,
                    amt,
                    Waveform::Sine,
                    Waveform::Sine,
                    &pw,
                    &pw,
                    &mut a,
                    &mut b,
                );
            }
            let mut buf = Vec::with_capacity(NFFT);
            for _ in 0..NFFT {
                o1.process_pm(
                    &mut o2,
                    amt,
                    Waveform::Sine,
                    Waveform::Sine,
                    &pw,
                    &pw,
                    &mut a,
                    &mut b,
                );
                poly_sub_square(&o1.phase, &o1.inc, &o1.sub_flipflop, &mut sub);
                buf.push(sub[0]);
            }
            assert_eq!(peak_bin(&buf), K / 2, "amt {amt}: sub pitch moved");
        }
    }

    #[test]
    fn sub_pitch_under_sync_locks_to_master_half() {
        // Sync path uses `process_sync`; the flipflop toggles on osc2 (master)
        // wraps, and the sub is read using osc2's phase/inc. Sub pitch = osc2/2,
        // independent of osc1's tuning above master.
        const SR: f32 = 48_000.0;
        const NFFT: usize = 1024;
        const K_M: usize = 32;
        let f_m = K_M as f32 * SR / NFFT as f32;
        let pw = [0.5; N];
        for ratio in [1.5_f32, 2.5, 3.7] {
            let f_s = f_m * ratio;
            let mut o1 = PolyOscillator::new();
            let mut o2 = PolyOscillator::new();
            o1.inc[0] = f_s / SR;
            o2.inc[0] = f_m / SR;
            let mut a = [0.0; N];
            let mut b = [0.0; N];
            let mut sub = [0.0; N];
            for _ in 0..NFFT {
                o1.process_sync(
                    &mut o2,
                    Waveform::Saw,
                    Waveform::Saw,
                    &pw,
                    &pw,
                    &mut a,
                    &mut b,
                );
            }
            let mut buf = Vec::with_capacity(NFFT);
            for _ in 0..NFFT {
                o1.process_sync(
                    &mut o2,
                    Waveform::Saw,
                    Waveform::Saw,
                    &pw,
                    &pw,
                    &mut a,
                    &mut b,
                );
                // Engine routes osc2 phase/inc into the sub under sync; the
                // flipflop lives on osc1 in all modes.
                poly_sub_square(&o2.phase, &o2.inc, &o1.sub_flipflop, &mut sub);
                buf.push(sub[0]);
            }
            assert_eq!(peak_bin(&buf), K_M / 2, "ratio {ratio}: sub not locked");
        }
    }

    #[test]
    fn sub_square_polyblep_beats_naive_aliasing() {
        // BLEP-smoothed sub has materially less high-band energy than the bare
        // comparator at the same flipflop pattern (mirrors the methodology of
        // `subsample_sync_beats_sample_accurate_aliasing`, scoped to the sub).
        const SR: f32 = 48_000.0;
        const NFFT: usize = 4096;
        let f_src = 1234.5_f32; // not bin-aligned, transitions land off-grid
        let mut osc = PolyOscillator::new();
        osc.inc[0] = f_src / SR;
        let pw = [0.5; N];
        let mut o = [0.0; N];
        let mut sub = [0.0; N];
        for _ in 0..NFFT {
            osc.process(Waveform::Sine, &pw, &mut o);
        }
        let mut blep = Vec::with_capacity(NFFT);
        let mut naive = Vec::with_capacity(NFFT);
        for _ in 0..NFFT {
            osc.process(Waveform::Sine, &pw, &mut o);
            poly_sub_square(&osc.phase, &osc.inc, &osc.sub_flipflop, &mut sub);
            blep.push(sub[0]);
            // Bare comparator — same flipflop, no BLEP residual.
            let sp = osc.phase[0] * 0.5 + osc.sub_flipflop[0] * 0.5;
            naive.push(1.0 - 2.0 * (sp >= 0.5) as u32 as f32);
        }
        let band = |x: &[f32]| -> f64 {
            (3 * NFFT / 8..NFFT / 2).map(|k| dft_mag(x, k)).sum()
        };
        let blep_hi = band(&blep);
        let naive_hi = band(&naive);
        assert!(
            blep.iter().all(|v| v.is_finite()),
            "non-finite sub samples"
        );
        assert!(
            naive_hi > blep_hi * 1.4,
            "BLEP didn't beat naive (blep {blep_hi}, naive {naive_hi})"
        );
    }

    #[test]
    fn sub_no_op_with_zero_inc_stays_finite() {
        // Frozen lane (inc = 0): no wraps, flipflop never toggles, sub stays
        // at the comparator's resting value (+1 since sp = 0 < 0.5).
        let mut osc = PolyOscillator::new();
        let pw = [0.5; N];
        let mut o = [0.0; N];
        let mut sub = [0.0; N];
        for _ in 0..200 {
            osc.process(Waveform::Pulse, &pw, &mut o);
            poly_sub_square(&osc.phase, &osc.inc, &osc.sub_flipflop, &mut sub);
            assert!(sub.iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn ring_mod_antisymmetric_and_finite() {
        // Negating either input negates the output; all lanes (incl. a frozen
        // zero lane) stay finite across a range of drives.
        let mut a = [0.0; N];
        let mut b = [0.0; N];
        let sig: [f32; N] = std::array::from_fn(|v| 0.3 + v as f32 * 0.05);
        let car: [f32; N] = std::array::from_fn(|v| 0.6 - v as f32 * 0.03);
        let neg_sig: [f32; N] = std::array::from_fn(|v| -sig[v]);
        for drive_db in [0.2_f32, 1.0, 6.0, 20.0] {
            let g = 10.0_f32.powf(drive_db / 20.0);
            poly_ring_mod(&sig, &car, g, &mut a);
            poly_ring_mod(&neg_sig, &car, g, &mut b);
            for v in 0..N {
                assert!(
                    a[v].is_finite() && b[v].is_finite(),
                    "non-finite @ {drive_db}"
                );
                assert!((a[v] + b[v]).abs() < 1e-5, "not antisymmetric in signal");
            }
        }
    }
}
