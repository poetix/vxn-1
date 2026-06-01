---
id: "0056"
title: vxn-app/engine — reverb globals + Type/Depth macro mapping
priority: high
created: 2026-06-01
epic: E012
---

## Summary

Add four global params (`reverb_on`, `reverb_type`, `reverb_depth`,
`reverb_mix`) to `vxn-app` and the engine-side macro helper that
resolves a `(Type, Depth)` pair into the six underlying knobs the
DSP engine actually consumes (`size`, `decay`, `damping`,
`mod_rate`, `mod_depth`, `jitter`).

This is the param-surface ticket only — no `Synth` field, no bus
wiring, no UI. Adding the IDs without wiring leaves them as inert
state; 0057 picks them up.

## Acceptance criteria

- [ ] `GlobalParam` enum gains `ReverbOn`, `ReverbType`,
      `ReverbDepth`, `ReverbMix` in
      `crates/vxn-app/src/params.rs`, inserted **after** `DelaySync`
      and **before** `LimiterOn` (keeps the FX cluster contiguous).
      Per [[vxn1-id-stability-dropped]], CLAP id renumbering is
      free.
- [ ] `GLOBAL_PARAMS` table grows by four entries with matching
      names:
      - `b("reverb_on", "Reverb", 0.0)`
      - `e("reverb_type", "Reverb Type", REVERB_TYPE_LABELS, 0.0)`
      - `f("reverb_depth", "Reverb Depth", 0.0, 1.0, 0.5, "",
        Taper::Linear)`
      - `f("reverb_mix", "Reverb Mix", 0.0, 1.0, 0.3, "",
        Taper::Linear)`
- [ ] `REVERB_TYPE_LABELS: &[&str; 4] = &["Plate", "Room",
      "Hall", "Large"]` defined in the same file.
- [ ] `GlobalValues::reverb_type() -> ReverbType` accessor (the
      enum-from-float pattern matching `lfo2_shape()`).
- [ ] `pub enum ReverbType { Plate, Room, Hall, Large }` in
      `crates/vxn-engine/src/reverb_macro.rs` (new file). `ALL`
      array, `from_index`, `index`.
- [ ] `pub struct ReverbVoicing { pub size: f32, pub decay: f32,
      pub damping: f32, pub mod_depth: f32 }` in the same module.
      `mod_rate` and `jitter` are not in the voicing — they stay
      fixed (mod_rate at upstream default 0.3, jitter at 0).
- [ ] `pub fn reverb_macro(t: ReverbType, depth: f32) ->
      ReverbVoicing` implementing the mapping table:

      | Type  | size_min | size_max | decay | damp | mod_depth |
      |-------|----------|----------|-------|------|-----------|
      | Plate | 0.10     | 0.30     | 0.55  | 0.30 | 0.10      |
      | Room  | 0.25     | 0.55     | 0.65  | 0.50 | 0.20      |
      | Hall  | 0.50     | 0.80     | 0.78  | 0.65 | 0.25      |
      | Large | 0.70     | 1.00     | 0.88  | 0.75 | 0.30      |

      `size = lerp(size_min, size_max, depth.clamp(0, 1))`.
- [ ] Unit tests in the new module:
      - `macro_size_lerps_within_type_range` — Depth=0/0.5/1
        produce the expected size for each of the four types.
      - `macro_fixed_per_type` — decay/damp/mod_depth match the
        table for each type, independent of depth.
      - `reverb_type_from_index_roundtrips` — mirrors the
        existing `from_index_roundtrips` test pattern.
- [ ] `param_tables_len_match_counts` test passes after the four
      new entries are appended.
- [ ] Workspace builds with the new params unwired (engine still
      ignores them — that's 0057).

## Notes

The table values are first-pass — tuned by ear in 0058 once the
panel is wired and audible. Keep them as `pub const` so a future
nudge is a one-line change.

`mod_rate` is parked at the upstream default (0.3) inside the
voicing helper because the user-facing knob set was deliberately
collapsed (E012 §Background). If a `Shimmer` knob lands later,
add `mod_rate` to `ReverbVoicing` then.

The `e(...)` helper in `GLOBAL_PARAMS` is the existing enum-param
constructor; check the LFO 2 Shape entry ([params.rs:876](crates/vxn-app/src/params.rs#L876))
for the call shape.
