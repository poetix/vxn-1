---
id: "0009"
title: MIDI event router & key mode
priority: high
created: 2026-05-25
epic: E003
---

## Summary

Route the inbound MIDI stream to the two layers according to `KeyMode` (ADR 0003
§3): round-robin (Whole), duplicate to both (Dual), or partition at the split
point (Split). Handle the seed-on-entry copy and mode-transition behaviour, and
store the split point as opaque saved state.

## Acceptance criteria

- [x] Note events are routed per `KeyMode`:
      - **Whole** — round-robin note-ons across the two layers (→ 16-voice poly,
        both layers reading layer A's params per 0008).
      - **Dual** — every note-on goes to **both** layers (layered, 8 each).
      - **Split** — note-on goes to Lower if `note < split_point`, else Upper.
      Note-offs follow their note-ons to the correct layer(s).
- [x] **Seed-on-entry:** transitioning Whole → Dual/Split copies layer A's
      per-patch values into layer B once (ADR 0003 §3), so Lower starts equal to
      Upper and then diverges.
- [x] `KeyMode` and **split point** are read from the non-automatable shared
      state defined in 0007 (not CLAP params), set discretely via the editor
      (0013). Split point is a MIDI note 0–127, default a sensible middle
      (e.g. 60).
- [x] Mode transitions and a moving split point do not strand voices: notes
      already sounding continue to their natural release on the layer that
      started them; only **new** note-ons follow the new routing.
- [x] Tests: Whole alternates layers across successive note-ons; Dual triggers
      both layers per note; Split directs notes by pitch about the split point;
      Whole→Dual seeds B from A; held notes survive a mode/split-point change.

## Notes

- Routing lives between the host event loop (`vxn-clap`) and the engine's
  per-layer note handling; keep it in the engine so it is testable without CLAP.
- `KeyMode` is set discretely (UI/state), not automated, so the seed-on-entry
  copy fires cleanly on the Whole→Dual/Split edge only; Dual↔Split does not
  re-seed. (A discrete edge is exactly why `KeyMode` is state, not a param —
  0007.)
- Round-robin counter is engine state; reset on `reset_all`.
- Depends on 0008. Validation: `cargo test -p vxn-engine`.
