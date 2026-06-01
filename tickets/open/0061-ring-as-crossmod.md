---
id: "0061"
title: Ring as a cross-mod option (drop RingLevel from mixer)
priority: medium
created: 2026-06-01
epic: E013
---

## Summary

Move ring modulation out of the mixer and into the `CrossModType`
selector as a fourth variant `Ring`. Remove the `RingLevel` patch
param and its mixer fader. When `CrossModType = Ring`, osc2
ring-modulates osc1 at full amplitude and the result is routed to
the osc1 mixer slot, so `osc1_level` controls the ring level. osc2
remains independently mixable via `osc2_level` (already true for
Sync and FM at [voice.rs:832](crates/vxn-engine/src/voice.rs#L832)).

## Design

- **`CrossModType` enum** ([crates/vxn-app/src/params.rs:101](crates/vxn-app/src/params.rs#L101))
  gains `Ring` as variant index 3. `COUNT = 4`. `ALL` extended.
  `from_index(3) => Ring`; other indices unchanged.
- **`CROSS_MOD_LABELS`** ([crates/vxn-app/src/params.rs:474](crates/vxn-app/src/params.rs#L474))
  becomes `&["Off", "Sync", "FM", "Ring"]`.
- **`PatchParam::RingLevel`** ([crates/vxn-app/src/params.rs:142](crates/vxn-app/src/params.rs#L142))
  removed. The `ring_level` `ParamDesc` row at
  [crates/vxn-app/src/params.rs:627](crates/vxn-app/src/params.rs#L627)
  removed. Per [[vxn1-id-stability-dropped]] the table is free to
  shift — no append-only discipline.
- **`BlockCtx::ring_level`** ([crates/vxn-engine/src/voice.rs:106](crates/vxn-engine/src/voice.rs#L106))
  removed. Replaced by a boolean `ring_mode: bool` on the ctx
  (computed engine-side from `CrossModType == Ring`).
- **Voice render** ([crates/vxn-engine/src/voice.rs:760](crates/vxn-engine/src/voice.rs#L760)):
  when `ring_mode` is on, the per-frame mix becomes
  ```
  mix[v] = ring[v] * ctx.osc1_level + o2[v] * ctx.osc2_level + …
  ```
  i.e. `ring` displaces `o1` in the osc1 slot. When off (any other
  cross-mod mode), the existing path runs unchanged and the ring
  buffer is not computed.
- **Cross-mod kernel selection**: today's engine picks
  `process_sync` / `process_pm` / `process` based on `sync` vs
  `pm_index`. Under Ring the oscillators are independent (no
  coupling at the kernel level — the ring is a *post-kernel*
  combination of `o1` and `o2`), so it takes the fast independent
  path. Add a new `ctx` boolean (or fold into the existing dispatch)
  such that `CrossModType::Ring` → fast path + ring sum into osc1
  slot.
- **`cross_mod_amount` dimming**: today `data-dim-unless-fm` dims
  the amount fader unless `cross_mod_type == FM`. Ring also has no
  amount (full amplitude always), so the existing dim rule already
  hides it under Ring. Verify in the UI; no code change expected
  beyond the label list (the dim predicate keys on `== FM`).

### Preset migration

Existing factory presets ship `ring_level` values:

```
crates/vxn-engine/presets/factory/Bass/FM Growl.toml:21:ring_level = 0.3
```

Migration policy: **drop `ring_level` from all preset TOMLs**.
Where `ring_level > 0` and `cross_mod_type` is not already set,
set `cross_mod_type = "Ring"` and bring `osc1_level` to the prior
ring level so the patch still rings at approximately the right
loudness. Where `cross_mod_type` is already Sync or FM and
`ring_level > 0`, **drop the ring contribution silently** (the new
model is mutually exclusive — sync + ring can't coexist) and note
the affected preset in the PR description so it can be re-voiced
in a follow-up if needed.

The preset loader treats absent fields as defaults, so removing
`ring_level` from a preset that didn't set it has no effect.

### HTML faceplate

[crates/vxn-ui-web/assets/faceplate.html:1125](crates/vxn-ui-web/assets/faceplate.html#L1125):
remove the `ring_level` fader from the Mixer panel body. The
remaining three faders (Osc1, Osc2, Noise) re-flow naturally.

[crates/vxn-ui-web/src/lib.rs:1429](crates/vxn-ui-web/src/lib.rs#L1429):
remove the `("fader", "ring_level", "Ring")` entry from the mixer
placeholder list.

The XMod panel's `cross_mod_type` button group will now render four
options instead of three; the existing `ButtonGroup` primitive
handles arbitrary variant counts.

## Acceptance criteria

- [ ] `CrossModType` has four variants; `from_index` and `ALL`
      cover them; `CROSS_MOD_LABELS` is `["Off","Sync","FM","Ring"]`.
- [ ] `PatchParam::RingLevel` removed; `PATCH_PARAMS` table no
      longer lists `ring_level`; no orphaned references compile.
- [ ] `BlockCtx::ring_level` replaced by `ring_mode: bool` (or
      equivalent enum field); the per-frame mix routes ring through
      `osc1_level` when on and pays nothing when off.
- [ ] Under `CrossModType::Ring` the oscillators take the
      independent fast kernel path (no `process_pair` /
      `process_sync` / `process_pm` call); ring output is summed
      into the osc1 slot at full amplitude.
- [ ] Under `Off` / `Sync` / `FM` the mix path is bit-identical to
      pre-change behaviour (osc1 in osc1 slot, osc2 in osc2 slot,
      noise unchanged) — verify with a fixed-seed render test.
- [ ] All factory `*.toml` files audited: `ring_level` removed
      everywhere; presets that needed it are migrated per the
      policy above.
- [ ] `cross_mod_amount` fader stays dimmed under `Ring` (verify
      it does — the existing `data-dim-unless-fm` predicate already
      handles this; no fader regression).
- [ ] Faceplate Mixer panel shows three faders (Osc1 / Osc2 /
      Noise); XMod Type button group shows four variants.
- [ ] `cargo test --workspace` passes; `cargo build --workspace`
      clean of warnings.

## Notes

ADR 0004 §3 / §4 prose ("**Mixer:** osc1, osc2, **ring**, noise…")
becomes stale; either edit the ADR in this ticket or open a follow-
up doc-only ticket. The ring DSP itself ([crates/vxn-dsp/src/poly.rs:595](crates/vxn-dsp/src/poly.rs#L595))
is unchanged — only its routing changes.

Tests at [crates/vxn-dsp/src/poly.rs:1254](crates/vxn-dsp/src/poly.rs#L1254)
(`ring_mod_zero_input_silences`, `ring_mod_nonzero_inputs_produce_output`,
`ring_mod_antisymmetric_and_finite`) still apply — they test the
kernel, not the routing.
