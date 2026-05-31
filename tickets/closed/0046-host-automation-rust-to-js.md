---
id: "0046"
title: Host automation sync — controller pushes ParamChanged events to WebView
priority: high
created: 2026-05-30
epic: E010
---

## Summary

Replace the placeholder `console.log` Rust→JS bridge from 0039 with
real, batched DOM updates. The controller emits `ParamChanged`
ViewEvents at the same cadence the Vizia editor's idle poll runs;
the WebView editor batches them per idle frame and calls a single JS
function that updates every affected control. This is what makes DAW
automation playback visible.

## Acceptance criteria

- [ ] On every controller tick that produced ViewEvents, the editor
      forwards a batch via a single `evaluate_script` call: `__vxn.applyViewEvents(json)`.
- [ ] JS dispatcher routes events by type: `ParamChanged` → look up
      control by ID, update its position + display string;
      `KeyModeChanged` → re-render keys area (deferred to E011's
      Vizia overlay for now); `EditLayerChanged` → re-bind layer-
      aware controls; `Status` → flash a status pill (lower-right
      corner).
- [ ] No DOM update arrives while a UI gesture for that param is in
      flight (the controller's echo-suppression rule from 0035
      already handles this; verify end-to-end with a recorded
      automation in a DAW alongside a manual fader drag).
- [ ] Batching keeps the bridge under 1000 events/sec under heavy
      automation. (One ParamChanged per affected param per tick;
      `TOTAL_PARAMS` ≈ 200, host tick ≈ 30 Hz → ~6000 events/sec
      worst case. Cap by deduping on `id` within a batch — keep
      the latest value.)

## Notes

`evaluate_script` IPC payloads are strings. Serialise with
`serde_json` (add as vxn-ui-web dep). For 0046, payload is a flat JSON
array of objects; the JS side already knows the dispatcher.

If a single batch ever exceeds a sane size (say 100 KB), split
across multiple `evaluate_script` calls. Practical concern only for
preset load, where every param changes.
