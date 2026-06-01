---
id: "0055"
title: vxn-dsp — port StereoVReverb (host-rate BBD tap-comb)
priority: high
created: 2026-06-01
epic: E012
---

## Summary

Extend `crates/vxn-dsp/src/bbd.rs` in-place with the host-rate
tap-comb reverb engine from `patches-bundles/patches-vintage`. One
mono BBD line, six MN3011 tap positions, two polarity tap-mixes
for decorrelated stereo, one feedback path through a damping LPF
with `fast_tanh` clipping, triangle clock LFO. No `true_bbd` sub-
sample engine in v1 (deferred — host-rate path only).

The primitives are already in this file (private):
`ContinuousPoleBank`, `default_pole_pairs`,
`normalised_pair_residues`, `DelayBuffer` with cubic read,
`OnePoleLpf`, `BoundedRandomWalk`. The new types are added beside
them and reuse via crate visibility — no public surface change
to the existing chorus path.

Self-contained — no engine, params, or UI work in this ticket.

## Acceptance criteria

- [ ] `TappedDelayLine` added to `vxn-dsp/src/bbd.rs`:
      single `DelayBuffer`, `process_tapped(x, full_delay_s) ->
      [f32; 6]` returning all six tap reads via `read_cubic` at
      MN3011 fractions (`[396, 662, 1194, 1726, 2790, 3328] /
      3328`), `set_saturation`, `set_jitter_amount`,
      `set_jitter_seed`. Mirrors the upstream
      `crate::mod_delay::TappedDelayLine` API.
- [ ] `pub struct StereoVReverb` added to the same file:
      one `TappedDelayLine` + two `ContinuousPoleBank` output
      recon banks (L/R), `OnePoleLpf` damping in the feedback
      path, triangle clock LFO state, `fb` register.
- [ ] `StereoVReverb::new(sample_rate, seed) -> Self`.
- [ ] `StereoVReverb::set_params(size, decay, damping, mod_rate,
      mod_depth, jitter)` — values in `[0, 1]` (decay ≤ 0.95),
      mapped internally to ms / Hz / coefficient ranges matching
      upstream `vreverb.rs` (`FULL_DELAY_MIN_MS=35`,
      `FULL_DELAY_MAX_MS=180`, `DAMP_FC_MIN_HZ=1200`,
      `DAMP_FC_MAX_HZ=8000`, `MOD_HZ_MIN=0.05`, `MOD_HZ_MAX=6.0`,
      `MOD_MAX_DEPTH=0.15`).
- [ ] `StereoVReverb::process_block(dry: &[f32], l: &mut [f32],
      r: &mut [f32])` — `dry` is the mono source; `l`/`r` receive
      the **wet** signal (mixing happens engine-side). Internal
      loop matches upstream `process` exactly: LFO advance →
      tap-comb → fb damping → recon banks → polarity tap-mixes.
- [ ] `StereoVReverb::reset()` clears delay buffer, fb register,
      LPF state, both recon banks.
- [ ] `pub use StereoVReverb` from
      `crates/vxn-dsp/src/lib.rs`.
- [ ] Four tests in a `#[cfg(test)] mod reverb_tests` ported
      verbatim from upstream `vreverb.rs::tests` (adapted for the
      wet-only output signature):
      - `dry_wet_zero_passes_only_dry` → asserts wet output is
        zero when input is zero with mix bypassed at caller side
        (or rephrase: assert wet output decays to zero with no
        input — the more meaningful sanity test now that mix is
        external).
      - `output_is_bounded_under_sustained_input` — 40 000
        samples of a 440 Hz sine, `decay = 0.9`, no NaN/Inf,
        `|wet| < 5.0`.
      - `impulse_tail_decays` — single impulse, hold delay
        steady (`mod_depth = 0`), late peak < early peak.
      - `taps_decorrelate_stereo` — short impulse train, max
        `|L - R|` > 1e-3 across 20 000 samples.

## Notes

The upstream file is `patches-bundles/patches-vintage/src/vreverb.rs`
(~520 lines, host-rate path is ~250 of those once the `Engine` enum
collapses to a single `TappedDelayLine`).

Constants stay associated with `StereoVReverb` (private), not
hoisted into module scope — keeps the BBD chorus's namespace
clean.

Existing `default_pole_pairs` / `normalised_pair_residues` /
`ContinuousPoleBank` / `OnePoleLpf` are all reused. Do **not**
duplicate them. The `recon_bank` helper in `bbd.rs` is already
suitable for the reverb's L/R output banks.

The wet-only block signature is the divergence from upstream: vxn-1's
FX bus does its own dry/wet blend (matches the `chorus_mix` /
`delay_mix` pattern), so `StereoVReverb` returns wet samples and the
engine does `l += mix * (wet_l - l)`. Simpler, fewer floats per
sample, lets `Mix` be smoothed gain-style in the engine.

`set_jitter_seed` is wired but the engine will call it once at
construction with a stable per-instance seed; `set_jitter_amount(0)`
stays the default until a future ticket exposes jitter.
