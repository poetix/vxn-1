---
id: E013
title: Mixer revamp — ring into cross-mod, sub osc into mixer
status: open
created: 2026-06-01
---

## Goal

Two linked changes to the oscillator/mixer surface:

1. **Ring moves out of the mixer, into the cross-mod selector.**
   `CrossModType` gains a fourth variant **Ring**. The Ring fader
   on the mixer panel disappears. When `CrossModType = Ring`, osc2
   ring-modulates osc1 at full amplitude and the result is routed
   through the osc1 mixer slot (`osc1_level` controls ring level).
   osc2's mixer slot (`osc2_level`) remains independent and always
   mixable, exactly as it already is for Sync and FM.
2. **A `Sub` slot is added to the mixer**, mixing in a square wave
   keyed to osc1's phase wrap — naturally one octave below osc1
   and phase-locked. Cross-mod interactions:
   - **FM (PM):** sub keys to the osc1 phase *accumulator* wrap
     (the unmodulated base phase), not the modulated read phase.
   - **Sync:** sub keys to the *master* (osc2) wrap, not the slave
     (osc1) wrap. In sync mode osc1's audible period equals osc2's,
     so the sub naturally sits an octave below the audible pitch.
   - **Ring / Off:** sub keys to osc1's free-running wrap.

Both strands share the mixer + `CrossModType` rewrite, so they
ride one epic.

## Background

- `osc2`'s mix level is already independent of how osc2 is consumed
  by the cross-mod circuit (sync master / PM modulator). The Sync
  and FM paths already write `o2[v]` to the mixer with `osc2_level`
  regardless of coupling mode ([crates/vxn-engine/src/voice.rs:832](crates/vxn-engine/src/voice.rs#L832)).
  No change needed for osc2 mixability.
- Today's Ring is a separate mixer channel with its own `RingLevel`
  fader, summed in alongside osc1/osc2/noise (0021 / ADR 0004 §3).
  In practice it competes for mixer width and reads as a "fourth
  source" when conceptually it is a *coupling mode* between osc1
  and osc2, like sync and FM. Folding it into `CrossModType` aligns
  the surface with the underlying model.
- VPolyDco (`patches-bundles/patches-vintage/src/vdco/core.rs`)
  already ships a Juno-style sub: a `sub_flipflop` toggled on each
  phase wrap, a sub phase derived as `lin_phase * 0.5 + (flipflop
  ? 0.5 : 0.0)`, PolyBLEP corrections on both the wrap and the
  half-cycle duty edge. The vxn-1 port can crib that arithmetic
  directly (SoA-ised across the lane loop).
- The cross-mod kernels split between an unmodulated phase
  accumulator and the read phase used to look up a waveform value
  ([crates/vxn-dsp/src/poly.rs:507](crates/vxn-dsp/src/poly.rs#L507)).
  This is exactly what the FM sub-keying rule needs: the sub's
  flip-flop reads the accumulator wrap, not the modulated read.

## In scope

- **0061 — Ring as a cross-mod option.** `CrossModType` gains a
  `Ring` variant; `RingLevel` and its mixer fader are removed.
  Engine routes the ring output through the osc1 mixer slot when
  the mode is Ring. Cross-mod amount dimmed/hidden under Ring.
- **0062 — Sub osc.** New `SubLevel` patch param; new `Sub` mixer
  fader. SoA sub kernel ported from VPolyDco (sub flipflop on
  accumulator wrap, PolyBLEP on the half-cycle square edges). Sub
  flipflop drive selected per `CrossModType`: osc2 wrap under Sync,
  osc1 accumulator wrap otherwise.

## Out of scope

- Sub octave / level range knobs beyond `Sub` level (Juno had only
  a level; same here).
- Sub waveform variants (only square — Juno behaviour).
- Ring drive / colour knob (still fixed at `RING_DRIVE_DB = 1.0`).
- Re-tuning factory presets that currently use `RingLevel` — the
  migration in 0061 is the minimum needed to keep them sounding
  similar; per-preset taste passes are out.
- ADR 0004 / 0006 prose updates beyond the necessary edits to keep
  the docs consistent with `CrossModType = {Off, Sync, FM, Ring}`.

## Tickets

- [ ] [0061 — Ring as a cross-mod option](../../tickets/open/0061-ring-as-crossmod.md)
- [ ] [0062 — Sub osc in the mixer](../../tickets/open/0062-sub-osc-mixer.md)

## Dependency order

```text
0061 (CrossModType += Ring; RingLevel rip-out) ──┐
                                                  ├─ both independent at the
0062 (SubLevel + sub kernel + xmod-aware keying) ┘  param level; 0062's sub-
                                                    keying logic must read
                                                    CrossModType in the same
                                                    enum shape 0061 lands.
```

0061 is the cleaner / smaller change and should land first because
it owns the `CrossModType` enum extension that 0062's sub-keying
switch reads. 0062 can land in parallel against a feature branch
that includes 0061's enum addition.

## Acceptance

- `CrossModType = Ring` produces audible ring-modulated osc1×osc2
  in the osc1 mixer slot at full amplitude; osc2 still mixes
  independently via `osc2_level`. The dedicated Ring fader is gone.
- `RingLevel` param is removed from the patch param table; existing
  presets with `ring_level > 0` migrate to `cross_mod_type = "Ring"`
  with osc1_level preserved (see 0061 acceptance for migration
  detail).
- `SubLevel = 0` is the cheap no-op path (no sub work performed).
- With `SubLevel > 0` and a steady osc1 saw, the sub produces a
  bit-stable square one octave below osc1, phase-locked through
  legato slides.
- Under FM the sub frequency is independent of cross-mod amount
  (keys the accumulator wrap, not the modulated read).
- Under Sync the sub frequency follows osc2 (master), so changing
  osc1 pitch while sync is engaged moves only the sync timbre, not
  the sub pitch.
- Under Ring the sub keys osc1's free-running wrap (osc1 is not
  reset by ring; behaves like Off for sub-keying purposes).
- No RT allocation; lane loop stays vectorised on all four cross-
  mod modes.
