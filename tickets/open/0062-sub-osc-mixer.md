---
id: "0062"
title: Sub osc — square one octave below, phase-locked, in the mixer
priority: medium
created: 2026-06-01
epic: E013
---

## Summary

Add a Juno-style sub oscillator to the mixer: a square wave keyed
to osc1's phase wrap, naturally pitching an octave below and
phase-locked to it. Implemented as a flip-flop toggled on each
phase wrap (so the sub period is exactly two osc1 periods).
Cross-mod interactions:

- **Off / Ring:** sub keys osc1's free-running wrap.
- **FM (PM):** sub keys osc1's *phase accumulator* wrap (the
  unmodulated base phase), not the modulated read phase. Sub
  frequency is independent of `cross_mod_amount`.
- **Sync:** sub keys *osc2* (master) wrap, not osc1 (slave). In
  sync mode osc1's audible period equals osc2's, so the sub
  naturally sits an octave below the audible pitch.

Anti-aliased with PolyBLEP on the sub square's edges, ported from
VPolyDco. Depends on [0061](0061-ring-as-crossmod.md) for the
`CrossModType::Ring` variant the sub-keying switch enumerates.

## Reference

`patches-bundles/patches-vintage/src/vdco/core.rs` implements
exactly this scheme for the mono / poly VDco:

- `VDcoVoice::sub_flipflop` toggles on each accumulator wrap
  (`advance`, line 172).
- `render_current` (line 200) derives the sub phase as
  `lin_sub_phase = lin_phase * 0.5 + (flipflop ? 0.5 : 0.0)` and
  the sub increment as `lin_dt * 0.5`. Sub waveform = square
  comparator at 0.5 with PolyBLEP on both the wrap and the half-
  cycle duty edge.
- `render_partial_sync` (line 299) resets `sub_flipflop = false`
  on hard sync — the same behaviour we want for vxn-1's sync mode
  (where the sub re-keys to master = osc2 wrap).

vxn-1's poly kernel already splits the accumulator phase from the
read phase under PM ([crates/vxn-dsp/src/poly.rs:507](crates/vxn-dsp/src/poly.rs#L507)),
which is exactly the hook the FM sub-keying rule needs.

## Design

### Param + UI

- New `PatchParam::SubLevel` in
  [crates/vxn-app/src/params.rs:127](crates/vxn-app/src/params.rs#L127),
  inserted next to `Osc2PulseWidth` / before `CrossModType` to keep
  the source-mix params grouped.
- `ParamDesc` row:
  ```rust
  f("sub_level", "Sub Level", 0.0, 1.0, 0.0, "", Taper::Linear),
  ```
- `BlockCtx::sub_level: f32` ([crates/vxn-engine/src/voice.rs:96](crates/vxn-engine/src/voice.rs#L96)).
- Faceplate Mixer panel ([crates/vxn-ui-web/assets/faceplate.html:1120](crates/vxn-ui-web/assets/faceplate.html#L1120)):
  add `<div class="ctl" data-control="fader" data-param="sub_level"
  data-label="Sub"></div>` after the `osc2_level` fader, so the
  mixer reads `Osc1 / Osc2 / Sub / Noise`. With 0061 removing the
  `Ring` fader, the mixer body returns to four faders.
- [crates/vxn-ui-web/src/lib.rs:1429](crates/vxn-ui-web/src/lib.rs#L1429):
  mirror the placeholder list entry.

### SoA kernel

Sub-osc state lives on `PolyOscillator`:

```rust
pub struct PolyOscillator {
    pub phase: [f32; N],
    pub inc:   [f32; N],
    sync_resid:   [f32; N],
    sync_pending: [f32; N],
    // New:
    sub_flipflop: [f32; N],   // 0.0 / 1.0; matches branchless lane loop
}
```

Reset zeros it ([crates/vxn-dsp/src/poly.rs:230](crates/vxn-dsp/src/poly.rs#L230)).

A new `poly_sub_square` free function in `vxn-dsp::poly` writes one
sub sample per voice given the lane's source `phase`, `inc`, and
`flipflop`. Branchless (mask-selected half-offset), with PolyBLEP
on the wrap edge and on the half-cycle duty edge — the same form
as `WPulse::sample` ([crates/vxn-dsp/src/poly.rs:155](crates/vxn-dsp/src/poly.rs#L155))
but with phase/inc halved and the comparator pinned at 0.5:

```rust
#[inline]
pub fn poly_sub_square(
    phase: &[f32; N],
    inc:   &[f32; N],
    flip:  &[f32; N],   // 0.0 or 1.0
    out:   &mut [f32; N],
) {
    for v in 0..N {
        let sp = phase[v] * 0.5 + flip[v] * 0.5;
        let sdt = inc[v] * 0.5;
        let naive = 1.0 - 2.0 * (sp >= 0.5) as u32 as f32;
        let pf = { let x = sp - 0.5 + 1.0; x - x.floor() };
        out[v] = naive + pblep(sp, sdt) - pblep(pf, sdt);
    }
}
```

### Flipflop drive

The flipflop must toggle on the *correct* wrap depending on cross-
mod mode. The cleanest hook is to compute the flipflop transition
in the same kernel that advances the phase that drives it, then
read the array out alongside `o1` / `o2`:

- **`process`** (fast independent path, used by Off / Ring / FM):
  toggle osc1's `sub_flipflop[v]` whenever osc1's accumulator wraps
  (`np_s >= 1.0`). Under FM the accumulator advances at base
  increment unchanged by PM — already true in
  [crates/vxn-dsp/src/poly.rs:557](crates/vxn-dsp/src/poly.rs#L557).
  Under Off, the same applies trivially.
- **`process_sync`** (sync path, used by Sync): toggle *osc1's*
  `sub_flipflop[v]` whenever the **master** (`other`) wraps. The
  flipflop is conceptually a property of the audible sub voice
  (one per polyphonic voice), so keeping it on `osc1` (the audible
  carrier) avoids an extra array lookup downstream — we just feed
  `osc2.phase` / `osc2.inc` into `poly_sub_square` for this mode.
- **`process_pm`** (PM path): toggle osc1's `sub_flipflop[v]` on
  the accumulator wrap.

The kernels currently advance phases inside their loops; the
flipflop toggle is one extra `xor` per lane per wrap, cheap.

### Voice render dispatch

`VoiceBank::render_block` ([crates/vxn-engine/src/voice.rs:630](crates/vxn-engine/src/voice.rs#L630))
adds, after computing `o1` / `o2`:

```rust
let sub_on = ctx.sub_level != 0.0;
if sub_on {
    let (sp, sdt, sflip) = match ctx.cross_mod_kind() {
        CrossModType::Sync => (&osc2.phase, &osc2.inc, &osc1.sub_flipflop),
        _                  => (&osc1.phase, &osc1.inc, &osc1.sub_flipflop),
    };
    poly_sub_square(sp, sdt, sflip, &mut sub);
    for v in 0..N { mix[v] += sub[v] * ctx.sub_level; }
}
```

(Borrow gymnastics aside — the actual implementation will need to
arrange the borrows so the `sub_flipflop` array can be read while
the kernels' `&mut` borrows are released. Either expose
`PolyOscillator::sub_state(&self) -> (&[f32;N], &[f32;N], &[f32;N])`
or do the sub render before the borrow-out for the next sample.)

`sub_level = 0` is the cheap no-op path: no kernel call, no PRNG-
adjacent work, identical bytes to today.

### Cross-mod mode threading

`BlockCtx` today exposes `sync: bool` and `pm_index: f32`. Add an
explicit `cross_mod: CrossModType` (or derive it from the existing
fields plus a new `ring: bool` from 0061) so the sub dispatch reads
the mode cleanly. Keep `sync` / `pm_index` for the kernel selection
path — those are pure data the kernels need anyway.

## Acceptance criteria

- [ ] `PatchParam::SubLevel` added; `PATCH_PARAMS` describes it
      (range 0..1, default 0, Linear taper, "Sub Level").
- [ ] `BlockCtx::sub_level: f32` plumbed through engine; default 0.
- [ ] `PolyOscillator` carries `sub_flipflop: [f32; N]`; reset
      clears it; new-instance default zero.
- [ ] `poly_sub_square` produces a band-limited square at half
      the source frequency; matches a scalar reference (sub
      derived from a `Oscillator` saw's wraps) within `1e-5` over
      4800 samples at multiple base frequencies, mirroring
      `poly_saw_matches_scalar_within_tolerance`.
- [ ] **Off / Ring:** sub frequency = osc1 / 2. Verify with a
      bin-aligned FFT of the sub-only output: peak at `f_osc1 / 2`.
- [ ] **FM:** sub frequency stays `osc1_accumulator / 2` regardless
      of `cross_mod_amount`. Verify by sweeping `cross_mod_amount`
      from 0 to 2 with a fixed osc1 pitch and confirming the sub
      pitch is constant.
- [ ] **Sync:** sub frequency = osc2 / 2. Verify by setting osc1
      well above osc2 with `CrossModType::Sync`, sweeping osc1, and
      confirming the sub stays locked to osc2 / 2.
- [ ] PolyBLEP active on both the sub wrap and the half-cycle
      duty edge — no broadband aliasing peak in the upper eighth
      of a 4096-bin FFT (mirror the methodology of
      `subsample_sync_beats_sample_accurate_aliasing` at
      [crates/vxn-dsp/src/poly.rs:1012](crates/vxn-dsp/src/poly.rs#L1012)).
- [ ] `sub_level = 0` is the cheap no-op path (no kernel call,
      no flipflop bookkeeping cost beyond the toggle that the
      cross-mod kernels do anyway).
- [ ] All frozen-lane / NaN guards intact: a lane with `inc = 0`
      under any cross-mod mode stays finite.
- [ ] `cargo test --workspace` passes.

## Notes

The flipflop is a single `f32` per lane (0.0 / 1.0) rather than a
`bool` so the lane loop stays branchless and vectorises — matches
`sync_pending` in the same struct.

VPolyDco also applies an "analog cap-charge curvature" shape to
the sub phase (`shape_phase` at [core.rs:75](patches-bundles/patches-vintage/src/vdco/core.rs#L75)).
vxn-1 doesn't use that on the main oscillators today, so skip it
for the sub too — keep the sub a clean band-limited square.

Sync softness is not in vxn-1's path (no equivalent of VPolyDco's
`render_and_advance_soft`). Sub flipflop reset on sync is therefore
unconditional in the sync kernel — the flipflop simply re-syncs to
the new master wrap as soon as one arrives.

No new ADR — this is a straightforward additive feature within
ADR 0004's existing mixer model, plus the cross-mod interaction
rules captured in the epic.
