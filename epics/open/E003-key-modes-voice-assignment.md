---
id: E003
title: Key modes & voice assignment
status: open
created: 2026-05-25
---

## Goal

Implement the Jupiter-8 **Whole / Dual / Split** key modes and the per-layer
**voice-assignment** model (poly / unison / portamento) on the architecture
settled in ADR 0003: two always-present layers (Upper/Lower), each a full patch
with its own LFO and modulation matrix; 8 channels per layer (16 total); key
mode expressed purely as **event-routing policy + parameter-source map**, never
a DSP reconfiguration.

This is the largest item on the roadmap and the structural turning point: VXN1
stops being mono-timbral. It also **absorbs Unison and Portamento** from ADR
0002 — they are the per-layer assignment model, not standalone features (ADR
0003 §4).

## Scope

**In:**

- Parameter model split into two per-patch blocks (`Upper_*` / `Lower_*`) plus a
  small global param block, with `KeyMode` + split point as non-automatable
  shared state, and one canonical serialization (state + future presets).
- Two-layer engine: two 8-channel layers, per-layer `BlockCtx` + LFO + matrix,
  two render passes summed into the (global) FX bus.
- MIDI event router: round-robin (Whole) / duplicate (Dual) / split-partition
  (Split), with seed-on-entry (copy layer A → B) and defined mode-transition /
  hanging-note behaviour. Split point as opaque saved state.
- Per-layer MIDI processor abstraction (poly first), with unison and portamento
  built on it, and a documented hook for future arpeggiation.
- Editor: key-mode selector, Upper/Lower edit-target toggle (hidden in Whole),
  split-point control.

**Out (deferred — ADR 0003 §Consequences):**

- Per-layer FX, separate Upper/Lower outs / per-layer pan (v1 = one global
  stereo FX bus).
- Per-end *assignment* of performance controls (v1 broadcasts bend/wheel values;
  each layer responds via its own routing params).
- A `BendRange` param (stays global ±2 st).
- Arpeggiation (hook only).

## Tickets

- [x] [0007 — Parameter model: per-patch blocks + global block](../../tickets/open/0007-param-block-split.md)
- [x] [0008 — Two-layer engine render](../../tickets/open/0008-two-layer-render.md)
- [x] [0009 — MIDI event router & key mode](../../tickets/open/0009-event-router-key-mode.md)
- [x] [0010 — Per-layer MIDI processor (poly)](../../tickets/open/0010-midi-processor-poly.md)
- [x] [0011 — Unison assign mode](../../tickets/open/0011-unison.md)
- [x] [0012 — Portamento](../../tickets/open/0012-portamento.md)
- [ ] [0013 — Editor: key mode, layer toggle, split point](../../tickets/open/0013-ui-key-modes.md)

## Dependency order

```text
0007 (param split) ──> 0008 (two-layer render) ──┬─> 0009 (event router/mode)
                                                  └─> 0010 (per-layer poly) ──┬─> 0011 (unison)
                                                                              └─> 0012 (portamento)
0008 + 0009 ──> 0013 (UI)
```

0007 is foundational and blocks everything. **Core deliverable** (modes working
with plain polyphony) = 0007–0010 + 0013. 0011 and 0012 complete the assignment
model and can slip to a follow-up without blocking the modes themselves.

## Acceptance

- Whole mode reproduces today's sound as 16-voice mono-timbral (both layers read
  layer A's params, round-robinned).
- Dual layers two independent patches across the keyboard (8+8); Split routes
  Lower/Upper patches either side of the split point (8/8).
- Entering Dual/Split from Whole seeds Lower from Upper once, then they diverge.
- Each layer has its own LFO and modulation matrix.
- Unison stacks a layer's 8 channels on one note with detune; portamento glides
  per layer.
- Every per-patch param is independently automatable with stable CLAP ids;
  `KeyMode` + split point persist in plugin state and are not automatable; one
  serializer covers state save/load (and is reusable for presets later).
- No RT allocation added; Whole-mode CPU is no worse than today (still 16
  channels total).
