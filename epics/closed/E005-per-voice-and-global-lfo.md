---
id: E005
title: Per-voice & global LFO split
status: closed
created: 2026-05-25
---

## Goal

Modernise VXN1's two LFOs into an **asymmetric pair**: **LFO 1 becomes
per-voice** (its own phase per note, retriggered at note-on, with a per-voice
delay→fade onset), and **LFO 2 becomes a single instrument-wide global LFO**
(one free-running phase shared across both layers and all 16 voices).

This **revises ADR 0003 §5** (LFO was "per layer, shared") and **reverses the
E004 deferral** of per-voice LFOs. It also **supersedes ticket 0016** (per-LFO
reset on note start): a per-voice LFO retriggers intrinsically at each note-on,
so a clean note-synced shape no longer means jolting a layer-shared phase. The
0016 commit has been reverted; its zero-crossing retrigger logic is folded into
0018.

Motivation: a single voice retriggering a *shared* LFO is incoherent — it
disturbs every other held note. Splitting the roles fixes that and matches how
classic + modern polysynths actually work: a per-voice LFO for per-note vibrato
with delay, and one global LFO for patch-wide movement (sweeps, tempo-synced
pulsing). The JP-8 had a single shared LFO; the per-voice LFO is the deliberate
modern addition (cf. the feature-roadmap divergences).

## Scope

**In:**

- **LFO 1 per-voice (0018):** move LFO 1 state out of `Synth` into each
  `VoiceBank` as per-channel phase (+ per-channel S&H PRNG), ticked once per
  control block per voice. Per-voice trigger at note-on retriggers to the
  shape's **zero crossing** (sine 0, tri 0.25, saws 0.5; square/S&H at the
  boundary), with a **free-run** toggle to keep the phase across note-ons.
  Replace the single `LfoDelay` with a per-voice two-stage **delay time** +
  **fade-ramp** onset. LFO 1 rate/shape/sync stay per-patch (host-sync from
  0015 resolves the shared rate, applied to every voice).
- **LFO 2 global / instrument-wide (0019):** a single `LfoCore` in `Synth`
  shared by both layers and all voices; sampled once per block and broadcast as
  the existing shared `lfo2_val`. Free-running, constant depth — **no delay**.
  Move `Lfo2Shape` / `Lfo2Rate` / `Lfo2Sync` from the per-patch block to the
  **global** block (a single oscillator has one rate/shape); the four `Lfo2*`
  **matrix-routing depths stay per-patch** (each layer routes the global LFO to
  its own destinations). Drop `Lfo2Delay`.
- Editor (E004 / 0017 interplay): LFO 1 panel gains delay-time / fade / free-run
  controls; LFO 2 controls move to a global section. Sequence after the engine
  tickets (see below).
- Docs: update ADR 0003 §5; note the consequence that the global LFO is
  instrument-wide shared state, not part of a per-layer patch/preset.

**Out (deferred):**

- Per-voice **rate spread / rate-CV** for LFO 1 (analog-style voice detune, à la
  `patches PolyLfo` `spread`) — possible follow-up, not required here.
- Making LFO 2 per-voice too, or a third LFO.
- Envelope time-scaling by key (ADR 0002 §5) — unrelated, still its own epic.
- Arpeggiation (hook-only per ADR 0003 §4).

## Tickets

- [x] [0018 — Per-voice LFO 1 (phase/trigger + delay & fade)](../../tickets/closed/0018-per-voice-lfo1.md)
- [x] [0019 — Global instrument-wide LFO 2](../../tickets/closed/0019-global-lfo2.md)

## Dependency order

```text
0018 (per-voice LFO 1) ──┐
0019 (global LFO 2)    ──┴─> E004/0017 editor (delay/fade/free-run + global LFO2 panel)
```

0018 and 0019 are largely independent (different LFOs); both touch the param
table and `build_ctx`, so land them in sequence to keep the table edits simple.
The editor work in E004/0017 should follow both so it surfaces the final
control set.

## Acceptance

- LFO 1 is per voice: voices started at different times have independent phases;
  a note-on retriggers only its own voice's LFO (held voices undisturbed). The
  per-voice trigger lands on the shape's zero crossing; a free-run toggle keeps
  the phase across note-ons.
- LFO 1's two-stage onset holds modulation at zero for `DelayTime`, then ramps
  over `Fade`; `DelayTime = Fade = 0` reproduces the immediate-full-depth path.
- LFO 2 is a single instrument-wide oscillator: both layers and all voices read
  one shared phase; its rate/shape/sync are global params; its routing depths
  are per-patch; it free-runs with no delay.
- Host-sync (0015) still resolves rates; sync-off is free Hz.
- No RT allocation; the no-LFO / zero-delay fast paths stay cheap; poly kernels
  stay finite for inactive voices.
- ADR 0003 §5 updated.
