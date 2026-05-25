---
id: "0019"
title: Global instrument-wide LFO 2
priority: high
created: 2026-05-25
epic: E005
---

## Summary

Make **LFO 2 a single instrument-wide global LFO**: one free-running phase
shared across **both layers and all 16 voices**, for patch-wide movement
(sweeps, tempo-synced pulsing). Counterpart to the per-voice LFO 1 (0018). A
single oscillator has one rate/shape, so **LFO 2's rate/shape/sync move to the
global param block**, while its four matrix-routing depths stay per-patch (each
layer routes the shared LFO to its own destinations).

## Where LFO 2 lives today (post-0015)

- LFO 2 is one of `Synth`'s `lfos: [[LfoCore; 2]; LAYERS]` — i.e. currently
  **per layer**, sampled per layer in `build_ctx` and passed as `lfo2_val` in
  `BlockCtx`.
- `Lfo2Shape` / `Lfo2Rate` / `Lfo2Sync` / `Lfo2Delay` and the `Lfo2*` matrix
  depths are all **per-patch** today.

## Design

- **Single shared core:** replace LFO 2's per-layer cores with one
  `lfo2: LfoCore` in `Synth`. Sample it **once per process block** (before the
  per-layer render loop) and broadcast the scalar to both layers' `BlockCtx` as
  the existing `lfo2_val`. Free-running; **no delay** (drop `Lfo2Delay`).
- **Rate/shape/sync → global:** move `Lfo2Shape`, `Lfo2Rate`, `Lfo2Sync` from
  `PatchParam` to `GlobalParam`. The 0015 host-sync `lfo_rate` resolution applies
  to the global LFO using these global params + the engine tempo. Reorder param
  tables freely (no id-stability constraint pre-release).
- **Routing depths stay per-patch:** the four `Lfo2Pitch/Cutoff/Amp/Pwm` matrix
  depths remain in the per-patch block, so each layer (and Whole/Dual/Split
  sourcing) routes the one shared LFO 2 to its own destinations with its own
  amounts. The matrix's `Lfo2` source row is unchanged (6×4).
- **build_ctx:** read `lfo2_val` from the shared core (passed in / read from
  `self`), not by ticking a per-layer core. Each layer still multiplies it by
  its own per-patch `Lfo2*` depths.
- **Reset:** the global LFO free-runs; engine `reset()` zeroes its phase. No
  note-on reset (that's the per-voice LFO 1's job, 0018).
- **Docs:** update ADR 0003 §5 and note the consequence — the global LFO is
  instrument-wide shared state, **not** part of a per-layer patch/preset (a
  single-layer preset won't carry LFO 2 rate/shape).

## Acceptance criteria

- [ ] LFO 2 is a single oscillator: both layers and all voices read one shared
      phase (verify the same `lfo2_val` reaches both layers' matrices in a block).
- [ ] `Lfo2Shape` / `Lfo2Rate` / `Lfo2Sync` are global params; the four `Lfo2*`
      routing depths remain per-patch and still drive all four destinations.
- [ ] Host-sync (0015) resolves the global LFO's rate from tempo; sync-off is
      free Hz. Absent host tempo, the 120 BPM fallback holds (no NaN).
- [ ] LFO 2 free-runs with constant depth — no per-voice/`Lfo2Delay` onset.
- [ ] With all `Lfo2*` depths at 0, output matches the no-LFO2 path.
- [ ] No RT allocation; poly kernels stay finite.

## Notes

- This is the back-compatible half of E005: a shared LFO 2 is close to today's
  per-layer behaviour, minus the per-layer rate/shape (now one global rate/shape)
  and the dropped delay.
- Land after or alongside 0018; both edit the param table, so sequence them.
- E004/0017 editor: LFO 2's shape/rate/sync controls move to a global panel
  section; its matrix-depth faders stay on the per-layer faceplate.
- Validation: `cargo test -p vxn-engine`.
