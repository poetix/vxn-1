---
id: "0010"
title: Per-layer MIDI processor (poly)
priority: high
created: 2026-05-25
epic: E003
---

## Summary

Introduce the per-layer **MIDI-event processor** abstraction (ADR 0003 §4): the
stage between a layer's routed event stream and its 8-channel allocation that
implements the assign mode. This ticket delivers the **poly** mode (today's
allocator, scoped to a layer's 8 channels) and establishes the seam that unison
(0011), portamento (0012), and future arpeggiation hang off.

## Acceptance criteria

- [x] A per-layer processor type owns the layer's note→channel allocation.
      Poly mode = first-free / oldest-steal across the layer's **8** channels
      (port the current `allocate` logic, bounded to `CHANNELS_PER_LAYER`).
- [x] New per-patch param `AssignMode` (enum; at least `Poly` now, with
      `Unison` reserved for 0011), appended within the per-patch block (0007).
- [x] The processor exposes a clean interface (`note_on` / `note_off` →
      channel(s)) so 0011/0012 add behaviour without touching the router (0009)
      or render (0008). Document an arpeggiation hook (stream transform before
      allocation) without implementing it.
- [x] Per-layer allocation is independent: layer A stealing a channel never
      affects layer B.
- [x] Tests: a layer plays up to 8 simultaneous notes then steals oldest;
      allocation is confined to the layer's channel range; behaviour matches the
      pre-refactor single-pool allocator when only one layer is active.

## Notes

- This is mostly a refactor of the existing allocator into a per-layer,
  per-assign-mode shape — the value is the **abstraction boundary**, which is
  what makes 0011/0012 cheap and keeps arp feasible later.
- Keep it allocation-free; the processor holds fixed `[_; 8]` state per layer.
- Depends on 0008 (layers exist) and reads routing from 0009. Validation:
  `cargo test -p vxn-engine`.
