---
id: "0014"
title: Second routable LFO (engine + params + 6×4 matrix)
priority: high
created: 2026-05-25
epic: E004
---

## Summary

Add a second full LFO per layer as a new modulation-matrix source, taking the
matrix from 5×4 to **6×4** (ADR 0002 §8). Each LFO is independent (own shape /
rate / delay) and per-layer (ADR 0003 §5), so this adds a second `LfoCore` and a
second fade-in to each of the two layers, plus a sixth `ModSource` row. CLAP id
stability is **not** required (pre-release, no presets in the wild), so the
param table is kept clean rather than append-only.

## Acceptance criteria

- [ ] `ModSource` gains `Lfo2` (6 sources; `COUNT = 6`), placed in source order
      (e.g. after `Lfo`); `ModSource::ALL` and the runtime source vector in
      `mod_sources()` updated to match. `ModMatrix` becomes 6×4.
- [ ] Matrix depth params become **24**, contiguous and source-major, via the
      existing `matrix_index()` formula (no special-casing). Reorder the
      `PatchParam` table freely to keep it clean.
- [ ] New per-patch params `Lfo2Shape` / `Lfo2Rate` / `Lfo2Delay`, mirroring the
      LFO1 descriptors (shape enum = 6 variants, rate 0.01–40 Hz log default 5,
      delay 0–4 s default 0), placed beside their LFO1 counterparts.
- [ ] `Synth` holds a second per-layer `LfoCore` (e.g. `lfos: [[LfoCore; 2];
      LAYERS]`), seeded with a distinct PRNG seed so S&H decorrelates from LFO1
      and across layers.
- [ ] `VoiceBank` gains a second `Lfo2FadeIn` (per voice, per layer); reset in
      `reset_all`.
- [ ] `BlockCtx` gains `lfo2_val` + `lfo2_delay`; `build_ctx` samples LFO2, sets
      its rate, and rebuilds the 6×4 matrix.
- [ ] `mod_sources()` inserts `lfo2_val * lfo2_fade.gain(v)` as the Lfo2 source
      so all four destinations see the faded LFO2.
- [ ] Tests: an Lfo2→cutoff (or pitch) depth modulates as LFO1 does; with all
      Lfo2 depths at 0 the output matches the pre-change path; matrix indexing
      round-trips for all 24 params.

## Notes

- Current LFO map: `LfoCore` in [lfo.rs](crates/vxn-dsp/src/lfo.rs);
  `ModSource`/`ModMatrix` in [modmatrix.rs](crates/vxn-engine/src/modmatrix.rs);
  per-layer LFO storage + `build_ctx` in [lib.rs](crates/vxn-engine/src/lib.rs)
  (~L67, L385–447); `LfoFadeIn` + `mod_sources()` in
  [voice.rs](crates/vxn-engine/src/voice.rs) (L32–80, L329–340).
- LFO2 is a plain free-running Hz LFO here; host-sync (0015) and reset (0016)
  layer on top and apply to *both* LFOs.
- Keep the no-LFO fast paths cheap: an LFO whose matrix row is all-zero still
  ticks (cheap) but contributes nothing.
- Validation: `cargo test -p vxn-engine`.
