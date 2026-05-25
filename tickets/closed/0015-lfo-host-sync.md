---
id: "0015"
title: Host tempo plumbing + LFO host-sync
priority: medium
created: 2026-05-25
epic: E004
---

## Summary

Wire host transport into the engine for the first time, and let each LFO
optionally lock its rate to host tempo at a musical subdivision instead of free
Hz (ADR 0002 §11, host-sync half). The CLAP `Process` struct is currently
ignored in `vxn-clap`; extract tempo (and the playing flag, for 0016) and feed
it to `Synth`. Applies per LFO, so to both LFO1 and LFO2.

## Acceptance criteria

- [ ] `vxn-clap` reads tempo (BPM) + transport state from the `Process` struct
      in `process()` (currently `_process`) and passes it to the engine each
      block; absent host tempo, fall back to a sane default (e.g. 120 BPM) and
      never produce NaN/Inf.
- [ ] `Synth` accepts tempo via a setter or per-block input and stores it for
      `build_ctx`.
- [ ] Per-LFO params: `LfoSync` (on/off) and `Lfo2Sync` (on/off). When on, the
      LFO's existing rate control is reinterpreted as a **subdivision index**
      (straight / dotted / triplet across a sensible range, e.g. 1/1…1/32);
      define the subdivision → multiplier table once and unit-test it.
- [ ] Synced rate = f(tempo_bpm, subdivision); set via `LfoCore::set_rate` in
      `build_ctx`. Sync off = free Hz, identical to today.
- [ ] Rate is clamped to the LFO's existing valid Hz range after conversion.
- [ ] Tests: for two tempi (e.g. 90, 140 BPM) and straight/dotted/triplet, the
      resolved LFO Hz matches the expected beat math; sync-off path is unchanged.

## Notes

- This introduces a transport dependency into a so-far transport-agnostic engine
  (ADR 0002 §Consequences) — keep it isolated to the rate computation; the LFO
  core stays Hz-driven.
- Reuse the same `LfoSync`/`Lfo2Sync` shape for the subdivision; the UI (0017)
  will swap the rate control's display between Hz and note values.
- Only the LFOs sync here — delay/chorus FX tempo-sync is out of scope.
- Validation: `cargo test -p vxn-engine`.
