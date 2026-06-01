---
id: "0057"
title: vxn-engine — wire StereoVReverb into the FX bus (post-delay)
priority: high
created: 2026-06-01
epic: E012
---

## Summary

Add the reverb engine to `Synth`, resolve its voicing each block
from the macro helper, and insert it into the FX chain between
Delay and the master Limiter. Mix glides gain-style through the
existing `ParamSmoother`. Type switches reset the engine to
avoid bleeding the previous voicing's tail into the new room.

Depends on 0055 (`StereoVReverb` exists) and 0056 (`ReverbType`,
`reverb_macro`, globals exist).

## Acceptance criteria

- [ ] `crates/vxn-engine/src/lib.rs` `Synth` gains a `reverb:
      StereoVReverb` field; constructed in `Synth::new` with a
      stable seed (e.g. `0xBBD0_0040`).
- [ ] FX bus insertion at
      [crates/vxn-engine/src/lib.rs:448](crates/vxn-engine/src/lib.rs#L448),
      between the delay block and the limiter:
      ```rust
      let reverb_on = self.params.global().bool(GlobalParam::ReverbOn);
      if reverb_on {
          let mut dry_in = [0f32; CONTROL_BLOCK];
          let dry_in = &mut dry_in[..block];
          for i in 0..block { dry_in[i] = 0.5 * (l_out[i] + r_out[i]); }
          let mut wet_l = [0f32; CONTROL_BLOCK];
          let mut wet_r = [0f32; CONTROL_BLOCK];
          let (wl, wr) = (&mut wet_l[..block], &mut wet_r[..block]);
          self.reverb.process_block(dry_in, wl, wr);
          for i in 0..block {
              let mix = self.smoother.next_reverb_mix();
              l_out[i] += mix * (wl[i] - l_out[i]);
              r_out[i] += mix * (wr[i] - r_out[i]);
          }
      }
      ```
- [ ] `update_effects` ([crates/vxn-engine/src/lib.rs:492](crates/vxn-engine/src/lib.rs#L492))
      resolves the macro voicing once per call and pushes the six
      underlying knobs to the engine:
      ```rust
      let t = g.reverb_type();
      let depth = g.get(GlobalParam::ReverbDepth);
      let v = reverb_macro(t, depth);
      self.reverb.set_params(v.size, v.decay, v.damping,
                             0.3, v.mod_depth, 0.0);
      ```
      (mod_rate fixed at 0.3, jitter parked at 0.)
- [ ] Type switch handling: in `update_effects` (or a sibling
      checked there), track the previous `ReverbType`; on change,
      call `self.reverb.reset()` **before** the next process block.
      A `reverb_was_type: Option<ReverbType>` field on `Synth`
      holds the previous voicing, identical pattern to
      `limiter_was_on` ([lib.rs:97](crates/vxn-engine/src/lib.rs#L97)).
- [ ] `ParamSmoother` extended:
      - `reverb_mix` glides gain-like (added to the existing
        per-block tick).
      - `reverb_depth` glides gain-like (so a depth automation
        sweep doesn't zipper the size resolution; `update_effects`
        reads the smoothed value).
      - `reverb_on` and `reverb_type` are not smoothed.
- [ ] Engine tests:
      - `reverb_off_passes_dry_unchanged` — `reverb_on = false`,
        FX chain output equals chain output with reverb feature
        absent (sample-exact for the dry path post-delay).
      - `reverb_type_switch_resets_tail` — feed impulse train
        with Type=Plate decay 0.8, switch to Type=Hall, assert
        the next block's wet output starts from a clean engine
        (no plate-tail samples ≥ ε in the first N samples post-
        switch).
- [ ] `cargo test --workspace` passes.

## Notes

Bus position is post-delay (decision recorded in E012). The
delay tail feeds the room — reverberating the echoes — which is
the more common chain order; matches Bitwig / Live's FX2 default.

`StereoVReverb::set_params` is called every `update_effects` tick
(Periodic cadence already). Cheap — it's just a couple of `exp` /
coefficient updates inside the engine. The DSP work is the
process loop, not the param push.

The two `[0f32; CONTROL_BLOCK]` stack buffers (~512 bytes each)
mirror the existing dry_buf pattern at
[lib.rs:427](crates/vxn-engine/src/lib.rs#L427) — same idiom, no
heap allocation.

If perf measurement on the 16-voice busy_profile shows the reverb
process loop is dominant, the next move is to bypass the recon
banks when `wet_l == 0 && wet_r == 0` for a full block (silent
input → silent fb → can skip). Not in this ticket.
