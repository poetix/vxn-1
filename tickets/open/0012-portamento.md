---
id: "0012"
title: Portamento
priority: medium
created: 2026-05-25
epic: E003
---

## Summary

Add per-layer **portamento** (pitch glide from the previous note to the new one)
in the MIDI processor (0010), with a per-patch time and on/off (ADR 0002 §4, ADR
0003 §4). Because it lives in the per-layer processor, Split/Dual get
independent glide per end for free.

## Acceptance criteria

- [x] Per-patch params: `PortamentoOn` (bool) and `PortamentoTime` (s, log,
      0 = instant), appended within the per-patch block.
- [x] Per-channel glide state: on note-on, the channel's pitch starts at the
      previous note's pitch and ramps to the target over the portamento time;
      time 0 reproduces today's instant pitch.
- [x] Glide is per layer/channel — Lower and Upper glide independently in
      Split/Dual.
- [x] Plays nicely with pitch bend, cross-mod and sync (glide affects the base
      pitch that feeds those, resolved at control-block rate consistent with
      `voice.rs`).
- [x] Tests: a legato note-on glides pitch over ~`PortamentoTime` to the target;
      time 0 jumps instantly (matches pre-change); glide is independent across
      layers.

## Notes

- v1 = simple per-channel one-pole/linear pitch ramp toward target, evaluated at
  control-block rate (where pitch is already resolved in `voice.rs`).
- JP-8's polyphonic portamento glides each voice from its last pitch to its new
  target; match that rather than mono legato-only glide.
- Per-end *assignment* (JP-8 UPPER-ONLY) is out of scope (ADR 0003 §Consequences)
  — each layer simply has its own on/off.
- Depends on 0010. Validation: `cargo test -p vxn-engine`.
