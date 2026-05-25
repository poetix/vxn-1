---
id: "0016"
title: LFO reset on note start
priority: low
created: 2026-05-25
epic: E004
status: superseded
---

> **Superseded by [E005](../../epics/open/E005-per-voice-and-global-lfo.md) /
> [0018](0018-per-voice-lfo1.md).** A shared-phase reset jolts every held voice;
> the per-voice LFO 1 retriggers intrinsically at each note-on instead. The
> committed 0016 work was reverted and its zero-crossing logic folded into 0018.

## Summary

Optionally retrigger an LFO's phase at note-on for repeatable per-note
modulation shapes (ADR 0002 §11, reset half). Per LFO, so both LFO1 and LFO2.
The LFO is per-layer (ADR 0003 §5), not per-voice, so "reset on note start"
restarts the shared layer LFO phase whenever a note starts in that layer —
documented as such.

## Acceptance criteria

- [ ] Per-LFO params `LfoReset` / `Lfo2Reset` (on/off, default off).
- [ ] When on, the layer's LFO phase resets at note-on (use the existing
      `LfoCore::reset()`); when off, phase free-runs as today.
- [ ] Reset is gated to the note-on path in the layer's voice allocation (the
      `note_on` flow in `VoiceBank`), reading the per-LFO flag from params.
- [ ] Reset-off reproduces today's free-running behaviour exactly.
- [ ] Test: with reset on, the LFO value at the first post-note-on block is the
      shape's phase-0 value (within tolerance) regardless of when the previous
      note ended; with reset off it continues the free-running phase.

## Notes

- Per-layer (not per-voice) reset means in poly the shared phase jumps on each
  new note-on — this is the intended JP-8-ish behaviour; note it in code.
- Small ticket; rides the note-on path. Independent of 0015 but both are
  per-LFO mode switches surfaced together in the UI (0017).
- Validation: `cargo test -p vxn-engine`.
