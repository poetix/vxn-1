# ADR 0004 — VXN1 osc-interaction polish & fixed-panel modulation

- **Status:** Accepted
- **Date:** 2026-05-25
- **Scope:** Two linked decisions for epic E006: (1) finishing oscillator
  interaction — band-limited hard sync, a ring modulator, and a unified
  cross-mod **type** control; and (2) replacing VXN1's generic modulation matrix
  with **fixed, labelled panels** in the JP-8/Juno idiom, including a split of
  pitch modulation into a vibrato-scaled common channel and a wide osc2-only
  channel for sync sweeps.

This ADR records *what* changes and *why*. Parameter ids and DSP internals are
settled by the E006 tickets (0020–0023) and the code; there is no CLAP
id-stability constraint pre-release.

## Context

ADR 0002 added hard sync and cross-mod (E002, landed). Two things from that work
were left rough:

- **Hard sync is sample-accurate, not sub-sample.** `vxn-dsp::poly::process_pair`
  resets the slave on the sample where the master wraps, so the reset jitters up
  to ~1 sample and the discontinuity sprays broadband aliasing (the
  `TODO(E002 follow-up)` minBLEP note). Notably, the *proper* fix already exists
  in `patches-dsp::oscillator` (`advance_wrap_frac` / `sync_reset(frac)` /
  `sync_blep_residual`) — VXN1's kernel was a stripped copy of it.
- **No ring modulator**, and sync/cross-mod are exposed as a bool + a knob with
  no clean "pick one" control. Cross-mod *depth* already works but as
  **exponential FM** (`exp2`/semitone), which drifts pitch with depth; we move it
  to through-zero phase modulation (decision 7). The type selector and ring mod
  are the other gaps.

Separately, the modulation UI is a generic **6-source × 4-dest matrix** (Env1,
Env2, LFO1, LFO2, Vel, Key → Pitch, Cutoff, Amp, PWM = 24 depth params). It is
flexible but unidiomatic for this instrument: it doesn't read like the JP-8/Juno
panel VXN1 is modelled on, and most cells stay at zero. We want fixed, labelled
routes that match how players actually patch this kind of synth.

## Decision

### 1. Band-limited hard sync (0020)

Port the three `patches-dsp` primitives into `process_pair`: extract the
master's sub-sample wrap fraction, reset the slave to `(1-frac)·inc` at that
fractional instant, and add a polyBLEP residual across the reset edge. The
master `dt` for the fraction maths is simply the **base increment** — sync and
the PM path are mutually exclusive (decision 3), and PM never modulates the
increment anyway (decision 7), so the master phase always advances at its
unmodulated rate. The polyBLEP residual *is* the mild analog "softening" of the
reset edge; it is not a separate effect. Sync-off stays bit-identical to the
independent fast path.

### 2. Ring modulator (0021, amended by 0061)

Add osc1×osc2 ring modulation using the **Parker diode-bridge model**
(`patches-modules::modulators::ring_mod`, DAFx-11): `diode_block(sig+½c) −
diode_block(sig−½c)`, with a diode I–V polynomial + tanh shaping whose `drive`
sets near-linear vs harmonically-coloured behaviour. Ported to the SoA poly
kernel. Aliasing-prone like sync/PM; leans on the engine oversampling for v1.

**0061 amendment:** ring is exposed through `CrossModType::Ring` rather than
its own mixer fader. When engaged the ring signal displaces osc1 in the mixer
slot, so `osc1_level` sets ring loudness; osc2 stays independently mixable.
Ring, Sync and PM are mutually exclusive at the engine.

### 3. Cross-mod as a type selector

Replace the independent `OscSync` (bool) + `CrossMod` (amount) with
**`CrossModType` {Off, Sync, PM, Ring}** + **`CrossModAmount`**. The four
modes are mutually exclusive: Off = independent fast path (bit-identical),
Sync = the band-limited hard sync of decision 1, PM = the through-zero phase
modulation of decision 7 at the set index, Ring = the diode-bridge ring of
decision 2 routed into the osc1 mixer slot (0061). (The UI may keep an "FM"-
style label since players expect that name; the engine implements PM — see
decision 7.) We accept losing the (rarely useful) ability to run sync,
modulation and ring simultaneously in exchange for a clearer control.

### 7. Cross-mod is phase modulation, not exponential FM

The cross-mod (`PM`) mode is implemented as **through-zero phase modulation**,
replacing E002's exponential frequency modulation (`inc1 = base · exp2(xmod·o2)`).
osc2 modulates osc1 by **offsetting osc1's read phase**, while osc1's phase
accumulator advances at its unmodulated base increment:

```text
o1 = osc_sample(wave1, frac(phase1 + index·o2), pw1, base_inc)
phase1 = advance(phase1, base_inc)      // unchanged, constant dt
```

Rationale (PM ≡ FM spectrally for the timbres we want, DX-style):

- **No pitch drift.** Exponential FM of an asymmetric modulator (saw/pulse)
  raises the average frequency (Jensen), detuning the note as depth rises. PM
  adds a bounded phase offset that doesn't accumulate — pitch centre is stable.
- **Stable `dt`.** The carrier advances at the base increment, so the polyBLEP
  edge band-limiting stays valid/cheap and the sync maths (decision 1) never has
  to reason about a moving master `dt`.
- **Cheaper** — an add + wrap instead of `exp2` per sample.
- **Through-zero by construction:** the modulator is a read-time offset, not an
  integrated frequency that could clamp at the zero-frequency wall. The summed
  read phase therefore needs a **two-sided wrap** (`x - x.floor()`, handling
  negative excursions) so the read pointer can run backward through zero. The
  carrier accumulator keeps its one-sided wrap; only the **modulated read** wraps
  two-sided.

Trade-offs accepted: the `amount` becomes a phase-deviation **index** (radians /
cycles), not semitones; the timbre differs from the old exp2 cross-mod
(pre-release, fine); PM of a hard-edge carrier is still phase distortion (Casio
CZ) and aliases on the moving edge → **sine-carrier bias** + oversampling remain
the aliasing levers. minBLEP for the modulation path is **not** pursued: it
corrects discontinuities, not the FM/PM sideband foldback, so it is the wrong
tool here.

### 4. Fixed-panel modulation (rip out the matrix)

Remove the 24-cell matrix (`ModSource`/`ModDest`/`ModMatrix`, `MATRIX_BASE`,
`matrix_index`) and replace it with fixed routes carrying **per-channel source
selectors**:

| Channel (dest)            | LFO source            | Env source            | Extra             |
| ------------------------- | --------------------- | --------------------- | ----------------- |
| Pitch (both osc, vibrato) | {Off/LFO1/LFO2}+depth | {Off/Env1/Env2}+depth | Pitch-wheel depth |
| PWM                       | {Off/LFO1/LFO2}+depth | {Off/Env1/Env2}+depth | —                 |
| Cutoff                    | {Off/LFO1/LFO2}+depth | {Off/Env1/Env2}+depth | Velocity depth    |
| Osc 2 pitch (wide)        | —                     | {Off/Env1/Env2}+depth | mod-wheel         |

> **The Cutoff row is amended by the E006 faceplate pass** — see the amendment at
> the foot of this ADR. Its source selectors are dropped: velocity, LFO 1, LFO 2
> and Env 1 each get their own fixed depth into cutoff. Pitch / PWM / Osc 2 pitch
> keep their selectors.

Consequences:

- **VCA is hardwired to Env2** — the Amp destination disappears entirely.
- **Key→cutoff** becomes a dedicated filter **key-track on/off**, defined as
  exactly **1 octave of cutoff per octave of key relative to C4** — cutoff is
  unchanged at C4, rises above it and falls below it (not a free matrix depth).
- **LFO2's routing survives** purely through the per-channel {Off/LFO1/LFO2}
  selectors — no dedicated LFO2 cells. Either LFO can feed any channel.
- The **mod-wheel is its own panel**, independent of the per-channel selectors:
  **mod→PWM, mod→cutoff, mod→reso, mod→Osc2 pitch**. This replaces today's single
  `ModWheelDest` selector.

### 5. Pitch is two destinations

The **common Pitch** channel is **vibrato-scaled** (narrow range, ~±12 st) and
moves **both** oscillators — it is for vibrato, not sweeps. A **separate wide
Osc 2 pitch** destination (octave range, ~±48 st, **osc2 only**) drives
sync/cross-mod timbral sweeps; it is fed by its own env selector + depth and by
the mod-wheel. Both fold into osc2's increment via the same exp2/semitone path
as coarse/fine/octave, so a sync patch can sweep osc2 across octaves while
vibrato stays gentle on both oscillators. (Range values are starting points,
tunable during 0022.)

### 6. Noise: drop brown

`NoiseColor` becomes **White/Pink** only (matches the two-button mixer
selector). Brown noise and its filter state are removed.

## Panel layout

- **Osc 1:** wave, octave/coarse/fine, PW.
- **Osc 2:** wave, octave/coarse/fine, PW, cross-mod type {Off/Sync/PM/Ring} + amount.
- **Osc mod:** Pitch (vibrato, both osc) ← LFO/env (+pitch-wheel); PWM ←
  LFO/env; Osc 2 pitch (wide) ← env.
- **Mixer:** osc1 / osc2 / noise levels + noise type (White/Pink, two
  buttons). Ring lives on the Osc 2 panel's cross-mod selector (0061) —
  engaging it displaces osc1 in this strip.
- **Filter:** HP cutoff, LP cutoff, resonance, drive, key-track on/off.
- **Filter mod:** Cutoff ← velocity / LFO 1 / LFO 2 / Env 1 (four fixed depths;
  no source selectors — see amendment).
- **Mod wheel:** mod→PWM / cutoff / reso / Osc2 pitch (octave range).

## Consequences

- **Less flexible, more idiomatic.** Arbitrary source→dest routings (e.g.
  velocity→PWM, key→amp) are no longer possible. We judge those low-value for
  this instrument; the fixed routes cover the musically common cases and read
  like the hardware panel.
- **Foundational table rewrite (0022).** The param table and `build_ctx` routing
  loop are rewritten once; 0021 (RingLevel) and 0023 (UI) build on it. No CLAP
  id-stability concern pre-release.
- **Engine routing simplifies** from a generic `source × dest` loop to explicit
  per-channel resolution; the VCA/amp path and key-track become hardwired terms.
- **ADR 0003 §5** (modulation matrix description) is superseded by the fixed
  routes here.
- Sync/PM/ring all remain aliasing-prone at extremes and rely on oversampling;
  minBLEP is pursued **only** for sync (a discontinuity); it is the wrong tool
  for PM/FM sideband foldback (decision 7), so PM leans on sine-carrier bias +
  oversampling instead.

## Dependency order

```text
0020 (BLEP sync) ── independent (DSP only) ──┐
0022 (param/routing rewrite) ──> 0021 (ring) ──> 0023 (UI)
```

## References

- ADR 0002 — feature roadmap (hard sync, cross-mod, HPF, second LFO).
- ADR 0003 §5 — modulation matrix (superseded by the fixed routes here).
- `patches-dsp::oscillator` — sub-sample wrap fraction + polyBLEP sync reset.
- `patches-modules::modulators::ring_mod` — Parker DAFx-11 diode-bridge ring mod.
- Epic E006 + tickets 0020–0023.

## Amendment — 2026-05-26 (fixed-source cutoff route; faceplate reorg)

The E006 faceplate reorg simplifies the **Cutoff** channel from §4's selector
model to **fixed sources**. §4's Pitch / PWM / Osc 2 pitch routes stand
unchanged (they keep their `{Off/LFO1/LFO2}` / `{Off/Env1/Env2}` selectors).

### Cutoff route — fixed sources

The Cutoff row of §4 loses both source selectors. Velocity, LFO 1, LFO 2 and
Env 1 each carry their **own depth** into cutoff:

```text
cutoff_mod = lfo1·d_lfo1 + lfo2·d_lfo2 + env1·d_env + vel·d_vel + key_track + wheel
```

- **Env → cutoff is always Env 1.** Env 1 is the assignable mod env (and still
  reaches its other destinations via their selectors); **Env 2 stays the VCA
  env** (§4 consequence "VCA hardwired to Env2" is unchanged).
- Params: `CutoffLfoSrc` / `CutoffEnvSrc` removed; `CutoffLfoDepth` splits into
  `CutoffLfo1Depth` + `CutoffLfo2Depth`; `CutoffEnvDepth` / `VelCutoffDepth`
  kept. The Filter Mod panel is now a plain four-fader row (Vel / LFO1 / LFO2 /
  Env1), not a route-column layout.
- **LFO 2's cutoff routing no longer rides §4's selector** — it has its own
  dedicated cutoff depth instead. Its routing to Pitch / PWM still goes through
  those channels' selectors.

### Faceplate row order

The panel rows are re-laid (UI only, no param change):

1. LFO 1, LFO 2, Osc 1, Osc 2, Mixer
2. Env 1, Env 2, Filter, Filter Mod
3. Pitch Mod, PWM Mod, Cross Mod, Mod Wheel, Pitch Wheel
4. Keys, Voice, Chorus, Delay, Master

Chorus / Delay move their on/off into the panel header (a toggle on the orange
title bar, left of the title) rather than a cell in the control row.

## Amendment — 2026-06-01 (osc1=carrier convention; wide channel = X-Mod sweep)

§3 and §5 are amended so that **osc1 is always the carrier** (the modulated /
audible side) across both cross-mod modes, and the wide pitch route becomes a
mode-gated **Cross-Mod Sweep** channel rather than an osc2-only channel.

### Sync convention flipped (§3)

Sync now treats **osc1 as the slave/carrier** (its phase is reset by osc2's
wrap; its waveform is the audible sync timbre) and **osc2 as the master** (its
wrap drives reset; its waveform is typically inaudible or low-mix). This matches
PM mode where osc1 is already the carrier whose read phase is offset by osc2.

Net: across Off / Sync / PM, osc1 is *always* the audible carrier and osc2 is
*always* the silent modulating signal (driver). The `process_pair` /
`process_sync` / `process_pm` kernels in `vxn-dsp::poly` are rewritten so
`self`=osc1=carrier-or-slave and the second arg=osc2=modulator-or-master.

### Wide channel renamed: Osc 2 Pitch → X-Mod Sweep (§5)

The wide (octave-range) pitch route is renamed and made mode-aware. Its target
follows the cross-mod mode:

| Mode | Wide channel target               | Why                                                |
| ---- | --------------------------------- | -------------------------------------------------- |
| Off  | both osc1 + osc2                  | env-driven whole-note pitch effects                |
| Sync | osc1 only (slave/carrier)         | sweeping the slave creates the sync sweep timbre   |
| PM   | osc2 only (modulator)             | sweeping the modulator sets the FM index/spectrum  |

Params: `Osc2PitchEnvSrc` / `Osc2PitchEnvDepth` / `ModWheelOsc2Pitch` →
`CrossModSweepEnvSrc` / `CrossModSweepEnvDepth` / `ModWheelCrossModSweep`. TOML
keys: `osc2_pitch_env_{src,depth}` / `mod_wheel_osc2_pitch` →
`cross_mod_sweep_env_{src,depth}` / `mod_wheel_cross_mod_sweep`. UI labels
become "X-Mod Env" / "X-Mod Dep" / "Wheel→X-Mod".

### Factory preset migration

Sync presets (Sync Lead, Mark Will Sync Us, Sync Hole, Glass Pad, Clean Sweep,
Great Divide) had osc1↔osc2 swapped (wave / coarse / fine / octave / level /
pulse_width) so the audible patch is preserved under the new convention. The
wide-channel keys were also renamed. One Off-mode preset (Tropical Pluckstorm)
that used the wide channel for an osc2-only pluck pitch effect now drops both
oscs together — the patch is rebalanced, change accepted.
