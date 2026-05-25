---
id: "0018"
title: Per-voice LFO 1 (phase/trigger + delay & fade)
priority: high
created: 2026-05-25
epic: E005
---

## Summary

Make **LFO 1 per-voice**: each note runs its own LFO 1 phase, retriggered at its
own note-on, with a per-voice **delay → fade** onset. This fixes the incoherent
shared-phase reset (a single voice could jolt the layer-shared LFO for every
held note — see the reverted 0016) and delivers authentic per-note vibrato. LFO
2 stays global (see 0019).

Reference: `patches-modules::modulators::poly_lfo::PolyLfo` — per-voice phase
array, per-voice `sync` trigger (sub-sample fractional reset), per-voice S&H
PRNG.

## Where LFO 1 lives today (post-0015)

- `Synth` owns `lfos: [[LfoCore; 2]; LAYERS]` and samples each **once per
  control block** in `build_ctx`, passing scalar `lfo_val` (LFO 1) into
  `BlockCtx`, held across the block.
- `VoiceBank::mod_sources(v, …)` reads that shared `lfo_val`. A per-voice
  fade-in already exists: `LfoFadeIn`, a gain ramp 0→1 over `LfoDelay` seconds.

## Design

- **Move LFO 1 into `VoiceBank`** as per-channel phase: `lfo1_phase: [f32; N]`
  plus per-channel S&H PRNG state/value (decorrelated seeds per channel). Tick
  once per control block per voice (control-rate; held across the block's
  frames) — no per-sample cost. `LfoCore` may stay as a per-voice helper or be
  inlined as a small poly phase bank in the voice bank.
- **Per-voice trigger:** in `VoiceBank::trigger` (the note-on seam), retrigger
  the channel's LFO 1 phase to the shape's **zero crossing** (reuse 0016's
  `LfoShape::zero_crossing_phase`: sine 0, tri 0.25, saws 0.5; square/S&H at the
  cycle boundary). A per-LFO **free-run** toggle (`Lfo1FreeRun`, default off =
  retrigger) skips the reset so the per-voice phase persists across that
  channel's note-ons.
- **Two-stage onset, per voice** (replaces `LfoDelay` / the `LfoFadeIn` single
  ramp):
  - `Lfo1DelayTime` (s): hold LFO 1 depth at zero this long after note-on.
  - `Lfo1Fade` (s): then ramp depth 0→1 over this duration.
  - `DelayTime = Fade = 0` pins depth to full immediately, reproducing today's
    output exactly. Keep allocation-free; the 0/0 case stays branch-cheap.
- **Per-voice value into the matrix:** `mod_sources(v, …)` reads the voice's own
  LFO 1 value × its onset gain instead of the shared scalar. Remove `lfo_val`
  (LFO 1) and `lfo_delay` from `BlockCtx`; pass LFO 1's shape/rate/sync + the
  delay/fade controls so the bank can tick and shape its own phases. (`lfo2_val`
  stays in `BlockCtx` — see 0019.)
- **Rate & host-sync:** LFO 1 rate/shape/sync stay per-patch. Resolve the
  per-block Hz once via the 0015 `lfo_rate` helper (free Hz, or synced
  subdivision from tempo) and apply it to every voice's increment.
- **Params:** keep `LfoShape` / `LfoRate` / `LfoSync` (per-patch). Replace
  `LfoDelay` with `Lfo1DelayTime` + `Lfo1Fade`; add `Lfo1FreeRun`. Reorder the
  `PatchParam` table freely (no id-stability constraint pre-release).
- **Docs:** update ADR 0003 §5 (LFO 1 now per voice) and the `VoiceBank` /
  `LfoFadeIn` module docs.

## Acceptance criteria

- [ ] LFO 1 phase is per voice: two voices started at different times have
      independent phases; a note-on retriggers only its own voice's LFO 1, never
      a held voice's. (Mirror `PolyLfo::sync_resets_per_voice`.)
- [ ] Per-voice trigger lands on the shape's zero crossing (sine 0, tri 0.25,
      saws 0.5; square/S&H at the boundary).
- [ ] `Lfo1FreeRun` on: a voice's LFO 1 phase persists across its note-ons.
- [ ] Two-stage onset: `Lfo1DelayTime` holds depth at zero, then `Lfo1Fade`
      ramps it in, per voice. `DelayTime = Fade = 0` reproduces the pre-change
      output exactly.
- [ ] Host-sync (0015) still resolves LFO 1's rate; sync-off is free Hz.
- [ ] No RT allocation; the zero-delay / no-LFO fast path stays cheap; poly
      kernels stay finite for inactive voices.
- [ ] `BlockCtx` no longer carries LFO 1's `lfo_val` / `lfo_delay`.

## Notes

- Performance: per-voice LFO 1 ticks once per control block per channel — 8
  channels × 2 layers = 16 updates/block, negligible.
- Stretch (deferred to a follow-up): per-voice **rate spread / rate-CV** for an
  analog-style voice detune, à la `PolyLfo`'s `spread`.
- Sequence before E004/0017 finalises the LFO 1 editor panel (delay-time / fade
  / free-run controls).
- Validation: `cargo test -p vxn-engine` (+ `-p vxn-dsp` if `LfoCore` changes).
