---
id: "0060"
title: VReverb — optional allpass diffusion stage
priority: medium
created: 2026-06-01
epic: E012
---

## Summary

Add a Schroeder-style allpass diffusion stage to `StereoVReverb`. The
upstream MN3011 tap-comb is deliberately sparse and metallic — six
recirculating taps with no diffusion. That's the right character for
Plate; less right for Hall/Large where listeners expect a smooth
density rather than a comb-flutter.

Two diffusers per channel, nested allpasses in series on the wet
signal, sit **after** the polarity tap-mix and **before** the output
recon bank. Out of the feedback loop, so the existing decay/damp
loop-gain tuning is unchanged. The amount is macro'd per voicing
(Plate dry-ish, Hall/Large drenched) — no user knob.

Schroeder allpass form:

```
y[n] = -g · x[n] + z⁻ᴺ · (x[n] + g · y[n])
```

`g ≈ 0.5–0.7`, two stages per channel with mutually prime delays in
the 4–15 ms range. Standard topology — flat magnitude, dispersive
phase, smears transients without colouring steady-state spectrum.

Depends on E012 0055–0058 (engine + UI live).

## Acceptance criteria

- [ ] New `AllpassDiffuser` struct in `crates/vxn-dsp/src/bbd.rs`,
      module-private alongside `TappedDelayLine`. Single-channel,
      single-stage: power-of-two ring buffer, integer delay (sample-
      accurate; no fractional read needed at the lengths we use).
      Schroeder form above. `set_params(delay_samples, g)`, `reset()`,
      `process(x) -> f32`.
- [ ] `StereoVReverb` gains four `AllpassDiffuser` instances —
      two per channel — and a `set_diffusion(amount: f32)` API.
      `amount = 0` collapses the diffusers to bypass (g = 0); `amount
      = 1` is the heaviest setting.
- [ ] Inside `process_block`, the diffusers run **after** the polarity
      tap-mix and **before** the output recon bank advance, on each
      channel independently. Order matters: tap-comb → diffuse →
      recon. Out of the feedback loop, so `decay`/`damping` keep
      their existing meaning.
- [ ] Stage delays: pick two mutually prime samples per channel,
      different across L vs R, in the 4–15 ms range at 48 kHz.
      Suggested starting set (tunable in this ticket):
      - L stages: 251, 419 samples (~5.2, 8.7 ms)
      - R stages: 311, 487 samples (~6.5, 10.1 ms)
      Stored as constants; not size-scaled in v1.
- [ ] `g` mapped from `amount` linearly to `[0.0, 0.7]` (Schroeder's
      stable ceiling).
- [ ] Macro mapping: extend `MacroRow` in
      `crates/vxn-engine/src/reverb_macro.rs` with a `diffusion: f32`
      field. Suggested per-type values:
      - Plate: 0.30 (mostly dry, keeps the metallic ring)
      - Room: 0.55
      - Hall: 0.75
      - Large: 0.85
- [ ] `ReverbVoicing` gains `pub diffusion: f32`; `reverb_macro`
      threads it through; the engine call passes it to
      `reverb.set_diffusion(v.diffusion)`.
- [ ] `macro_fixed_per_type` test extended to cover the new field.
- [ ] Tests in `bbd::reverb_tests`:
      - `diffusion_preserves_dry_wet_unity_at_zero` — `amount = 0`
        is bit-identical to a pre-0060 build for a fixed impulse +
        decay setup (snapshot a buffer at HEAD before the change,
        check the post-change run matches).
      - `diffusion_smooths_impulse_density` — with diffusion = 1,
        the count of zero-crossings in a 50 ms window after a single
        impulse is substantially higher than with diffusion = 0
        (loose threshold; the point is qualitative density change,
        not a numerical match).
      - `diffusion_does_not_blow_up` — sustained input at decay 0.9,
        diffusion = 1, bounded |output| over 40 000 samples.
- [ ] `cargo test --workspace` passes.

## Notes

Why not inside the feedback loop? Two reasons:

1. Loop gain interacts with `decay` × LPF × tanh — adding a diffuser
   stage in there means re-tuning every voicing's decay ceiling.
   The work isn't free, and the post-mix placement gets the headline
   "smoother tail" benefit without the retune.
2. The recirculation pickoff in the upstream design is the longest
   tap directly, by intent. Inserting a diffuser between the tap and
   the feedback adder changes the BBD-flavour character.

Why no user "Diffusion" knob? The macro UI is the whole point of E012.
A per-type bake keeps the surface at Type + Depth + Mix. If user
testing reveals one voicing wants two amounts ("Hall + dry diffusion"
vs "Hall + lush diffusion"), promote it to a knob in a follow-up.

Why integer delay? At the delays here (~5–10 ms) a sample-accurate
integer offset gives an impulse-response density change without any
phasing artifacts that a swept-fractional read might introduce. The
diffuser is *static* — no LFO modulation on its delays. Keeps the
mod-LFO breathing concentrated in the tap-comb where it belongs.

Out of scope: modulated allpass delays (Dattorro/Lexicon style),
nested-loop topology, per-channel decorrelation tuning beyond the
suggested L/R delay split. If E012 ships and there's still appetite
for a denser plate-style topology, that's a separate epic.
