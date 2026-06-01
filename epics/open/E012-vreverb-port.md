---
id: E012
title: VReverb port — third FX panel in row 4, macro UI
status: open
created: 2026-06-01
---

## Goal

Port the MN3011-style BBD reverb (`VReverb`) from
`patches-bundles/patches-vintage` into vxn-1 as the third effect on
row 4 of the faceplate, hidden behind a macro UI (Type + Depth +
Mix) rather than its native 7-knob surface.

A *flavour* reverb, not a clean hall — comb-filtered, sparse,
metallic, with the "metal-and-tile" ringing of a Doepfer A-188-2.
Pairs with the existing BBD chorus's character.

## Background

`patches-vintage::VReverb` is a single tapped BBD line with six
MN3011 tap positions, two polarity tap-mixes for decorrelated
stereo, recirculation through a damping LPF, and a triangle clock
LFO for room-breathing shimmer. `vxn-dsp::bbd` already hosts the
primitives the host-rate engine needs (`ContinuousPoleBank`,
`default_pole_pairs`, `DelayBuffer` cubic read, `OnePoleLpf`,
`BoundedRandomWalk`), ported during the chorus work — they're
private but in-crate, so the reverb engine can be added in-place
without bumping anything to `pub`.

The upstream parameter surface (`dry_wet`, `size`, `decay`,
`damping`, `mod_rate`, `mod_depth`, `jitter` + structural
`true_bbd`) is too wide for a small row-4 panel. Collapse to:

- **Type** — Plate / Room / Hall / Large (enum). Fixes decay,
  damp, mod_depth, and a per-type size range.
- **Depth** — lerps size within the type's range; longer & deeper
  as it climbs.
- **Mix** — wet/dry.

Plus a header `On` switch matching Chorus / Delay's idiom.

The `true_bbd` aliasing path (`TappedBbd` + `bbd_clock`) is
deferred — host-rate engine ships first; aliasing is a future
escape hatch if the v1 character is too clean.

## In scope

- `StereoVReverb` engine in `vxn-dsp::bbd`, host-rate path only,
  with the upstream tests ported verbatim.
- Four new globals: `reverb_on`, `reverb_type`, `reverb_depth`,
  `reverb_mix`. Macro mapping table lives engine-side.
- Engine bus wiring: post-delay, pre-limiter.
- Faceplate panel inserted between Delay and Master, with tuned
  row-4 flex shares.
- Factory preset audit — defaults off, tasteful per-type defaults
  on a handful of presets that benefit.

## Out of scope

- `true_bbd` structural toggle / `TappedBbd` aliasing engine.
- Jitter / mod-rate as user knobs (jitter parked at 0, mod-rate
  fixed at upstream default 0.3 → ~0.4 Hz).
- Pre-delay bus routing option (post-delay only).
- ADR. Worth one for the macro-UI precedent ("0008-vreverb-
  macro-ui.md") — left as an optional follow-up after 0058 lands
  if the macro idiom feels right.
- Cross-fade between voicings on a Type switch (current plan:
  hard switch + `reset()`).

## Phasing

- **0055** DSP — extend `vxn-dsp::bbd` with `TappedDelayLine`
  + `StereoVReverb` + tests. Self-contained.
- **0056** Globals + macro mapping — four new `GlobalParam`s in
  `vxn-app`, `ReverbVoicing` + `reverb_macro(type, depth)` helper
  engine-side.
- **0057** Engine bus — `Synth.reverb` field, post-delay
  insertion, `update_effects` resolves voicing via the macro
  helper, smoothing for `mix` + `depth`, hard switch + `reset()`
  on Type change.
- **0058** HTML faceplate — Reverb panel between Delay and
  Master, tuned row-4 flex shares, tests updated.
- **0059** Factory preset audit — defaults + tasteful per-type
  presets.

## Acceptance

- `cargo test --workspace` passes.
- Faceplate row 4 shows six panels: Keys / Voice / Chorus /
  Delay / Reverb / Master.
- Toggling Type in any factory preset audibly changes character;
  Depth audibly changes size; Mix audibly changes wet level.
- No tail bleed between Type switches (a `reset()` empties the
  line cleanly).
- New factory bank builds and loads.
