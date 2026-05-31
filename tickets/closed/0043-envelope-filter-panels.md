---
id: "0043"
title: HTML faceplate — Env 1 / Env 2, VCA, Filter, Filter Mod panels
priority: high
created: 2026-05-30
epic: E010
---

## Summary

Implement Row 2. Envelopes (ADSR + Shape), the VCA panel (Amp-LFO
source + depth + the env-bypass gate switch), the Filter (HPF, Cutoff,
Reso, Drive, Mode, Slope, KeyTrack), and the Filter Mod row (four
fixed-source depth faders into cutoff: Vel, LFO1, LFO2, Env1).

## Acceptance criteria

- [ ] Env 1, Env 2 panels: A, D, S, R faders + Shape rotary
      (linear/exp curves).
- [ ] VCA panel: AmpLfoSrc (dropdown), AmpLfoDepth (fader), AmpEnvBypass
      (switch).
- [ ] Filter panel: HPF (fader), Cutoff (fader), Reso (fader), Drive
      (fader), Mode (rotary), Slope (12/24 dB switch), KeyTrack
      (fader).
- [ ] Filter Mod panel: four faders (Vel, LFO1, LFO2, Env1) — no
      source selectors (E006: fixed sources).
- [ ] All controls layer-aware (placeholder per 0041).

## Notes

Filter Mode (LP / BP / HP / Notch / OTA-LP …) is the multi-variant
enum that doesn't fit a two-state switch; it should use the rotary
selector, matching Vizia. Confirm in vxn-engine descriptor.

Mode switching cascades a few visual hints in Vizia (slope dim
when mode doesn't take slope, etc.). Replicate as CSS class toggling
driven by `ParamChanged` events. No client state.
