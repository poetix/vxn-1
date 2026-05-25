---
id: E004
title: LFO expansion — second LFO & host-sync/reset
status: closed
created: 2026-05-25
---

## Goal

Finish ADR 0002's modulation section: add a **second full LFO** as a matrix
source (§8) and give every LFO optional **host-tempo sync** and **reset on note
start** (§11). This grows the modulation matrix from 5×4 to **6×4** and
introduces VXN1's first dependency on host transport.

Both LFOs are *full* LFOs (own shape / rate / delay), per ADR 0003 §5 living
**per layer** — so this lands on the two-layer engine settled in E003: each of
Upper/Lower gains a second `LfoCore` and a second matrix row. Sync and reset are
per-LFO mode switches (§11 "both apply per LFO").

These are the last two synthesis items left in ADR 0002. After E004 only the
independent **envelope time-scaling by key** (§5) remains on that roadmap.

## Scope

**In:**

- `vxn-engine`: a second per-layer `LfoCore` (`lfos: [[LfoCore; 2]; LAYERS]` or
  parallel arrays), its own `Lfo2FadeIn` per voice bank, `lfo2_val`/`lfo2_delay`
  in `BlockCtx`, sampled + rate-set in `build_ctx`.
- Matrix → **6×4**: add `ModSource::Lfo2` in source order and four new depth
  params, keeping the matrix block **contiguous and source-major** (the clean
  layout). CLAP id stability is *not* a constraint (pre-release, no presets in
  the wild), so the param table is reordered freely rather than append-only.
- New per-patch params `Lfo2Shape` / `Lfo2Rate` / `Lfo2Delay` placed beside
  their LFO1 counterparts.
- Host transport: extract tempo (+ playing flag) from the CLAP `Process` struct
  in `vxn-clap` (currently ignored) and feed it into the engine.
- Per-LFO **host-sync**: when enabled, rate control selects musical subdivisions
  (straight / dotted / triplet) locked to host tempo instead of free Hz.
- Per-LFO **reset on note start**: retrigger the layer LFO phase at note-on.
- Editor: LFO2 controls (shape/rate/delay), the new matrix row, and the
  sync/reset toggles, on the existing per-layer faceplate.

**Out (deferred):**

- Envelope time-scaling by key (ADR 0002 §5) — its own follow-up epic; unrelated
  to LFO/matrix.
- Per-voice (vs per-layer) LFOs — LFO stays per-layer (ADR 0003 §5); reset on
  note-on retriggers the shared layer phase, documented as such.
- Tempo-synced *delay/chorus FX* — only the LFOs sync here.
- Arpeggiation (still hook-only per ADR 0003 §4).

## Tickets

- [x] [0014 — Second routable LFO (engine + params + 6×4 matrix)](../../tickets/closed/0014-second-lfo.md)
- [x] [0015 — Host tempo plumbing + LFO host-sync](../../tickets/closed/0015-lfo-host-sync.md)
- [~] [0016 — LFO reset on note start](../../tickets/closed/0016-lfo-reset.md) — **superseded by [E005](../closed/E005-per-voice-and-global-lfo.md)** (per-voice LFO 1 retrigger)
- [x] [0017 — Editor: LFO2, matrix row, sync/reset controls](../../tickets/closed/0017-ui-lfo2.md) — surfaces the **E005** control set (per-voice LFO 1 delay/fade/free-run + global LFO 2 panel)

## Dependency order

```text
0014 (second LFO + 6×4) ──┬─> 0016 (reset on note)   ── both per-LFO mode switches
                          └─> 0017 (UI)
0015 (tempo plumbing + sync) ── needs transport; applies to both LFOs ──> 0017 (UI)
```

0014 is foundational (it establishes the 6×4 matrix and the per-layer second
LFO). 0015 brings in the transport dependency
and the subdivision rate model; 0016 is small and rides the note-on path. **Core
deliverable** (a usable second routable LFO) = 0014 + 0017. 0015 and 0016 are
per-LFO polish that apply to both LFO1 and LFO2 and can slip to a follow-up.

## Acceptance

- A second LFO modulates any of pitch/cutoff/amp/PWM via its own matrix row,
  per layer, with its own shape/rate/delay; LFO1 behaviour is unchanged.
- Matrix is 6×4; all 24 depth params are independently automatable, contiguous
  and source-major.
- With host-sync enabled, an LFO's rate tracks host tempo at the selected
  subdivision (verified across straight/dotted/triplet at two tempi); with sync
  off it is free-running Hz as today.
- With reset-on-note enabled, the layer LFO phase restarts at note-on for
  repeatable per-note shapes; with it off, phase free-runs.
- Tempo/transport reaches the engine from the CLAP `Process` struct; absent a
  host tempo, sync falls back to a sane default and never NaNs.
- No RT allocation; zero-depth Lfo2 and sync-off / reset-off reproduce today's
  output; poly kernels stay finite for inactive voices.
