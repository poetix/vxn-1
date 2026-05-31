---
id: "0041"
title: HTML faceplate — Osc 1, Osc 2, Mixer panels (incl. waveform selectors)
priority: high
created: 2026-05-30
epic: E010
---

## Summary

Implement the Row 1 oscillator and mixer panels in the HTML faceplate:
Osc 1 (Wave, Oct, Semi, Fine, PW), Osc 2 (same set), Mixer (Osc1,
Osc2, Ring, Noise levels + noise colour). Introduce the JS primitive
controls reused across every later panel: vertical fader, rotary
waveform selector, segmented button group, switch, dropdown. Each
control posts `UiEvent` on edit, listens for `ParamChanged` view
events from the controller.

## Status

**Complete.** Infrastructure (timer pump, descriptor push, dispatch)
plus all five primitives and the three Row-1 oscillator/mixer panels
landed across the 0041 + 0041a commits.

## Acceptance criteria

- [x] JS control primitives implemented:
      - [x] `Fader(id, label)` — vertical slider, pointer down/up
            brackets `BeginGesture` / `EndGesture`, drag posts
            `SetParamNorm`. Uses pointer-capture so the drag tracks
            past the track edges.
      - [x] `WaveformKnob(id, label)` — rotary selector with the same
            glyph set as `wave_points` (Sine, Tri, Saw, Pulse, etc.).
            Glyphs arranged on a -120°…+120° arc; click selects.
      - [x] `Switch(id, label)` — vertical toggle for bools; also
            renders a 2-variant enum (NoiseColor etc.) as two
            exclusive labelled toggles, matching vizia's
            `Ctl::Switch`. Click brackets begin/end gestures.
      - [x] `ButtonGroup(id, label, variants)` — for Oversample,
            CrossModType, AssignMode. Vertical stack of labelled
            toggles under the column label.
      - [x] `Dropdown(id, label, variants)` — native `<select>`
            fallback styled to the dark palette.
- [x] Osc 1 panel renders Wave (rotary), Oct (fader), Semi (fader),
      Fine (fader), PW (fader). Layer hard-bound to Upper; the
      Upper/Lower edit-layer toggle is deferred to 0045 along with
      the Voice panel.
- [x] Osc 2 panel — identical control set, different param IDs
      (`osc2_*`). Reuses the Osc 1 mount markup verbatim.
- [x] Mixer panel — four faders (Osc1, Osc2, Ring, Noise) + Col
      switch (White/Pink, two-variant enum). The Col switch sits in
      a new `.panel-strip` (absolutely placed at the bottom of the
      panel, mirroring vizia's bottom-strip layout) so it never
      competes for ctl-column flex space.
- [x] Each control's value display reads from the descriptor
      `display` string carried in the `ParamChanged` ViewEvent (the
      Vizia formatting routes through the same controller path).
- [x] DAW automation moves the right controls (Rust → JS push) via
      the CLAP `timer-support` extension. The clack shell registers
      a ~16 ms (60 Hz) main-thread timer in `set_parent`; `on_timer`
      ticks the controller and drains `view_rx` into
      `EditorHandle::push_view_event`.
- [x] UI gestures bracket correctly — Fader pointerdown posts
      `begin_gesture`, pointerup/cancel posts `end_gesture`; the
      WaveformKnob's discrete write is wrapped in a `begin/end`
      pair too so the host records one edit, not zero.

## Follow-up: 0041a (landed in this ticket)

- [x] Switch / ButtonGroup / Dropdown primitives.
- [x] Osc 2 panel.
- [x] Mixer panel (faders + NoiseColor switch in `.panel-strip`).

## Architecture notes (kept here for the follow-ups)

The bridge is established. Subsequent panel tickets only need to:

1. Drop control mount points into the panel body with
   `data-control="..." data-param="<descriptor.name>"
   data-label="..."`.
2. If the control type is new, add a `makeFoo(el, id, desc)` factory
   to `assets/faceplate.html` that builds DOM + binds events and
   returns `{ update(plain, norm, display) }`.
3. Add the kind to the `init()` switch.

No new Rust code required per panel; the descriptor JSON push +
ViewEvent dispatch cover routing automatically.

## Notes

The "layer-aware" question — how does the HTML editor know it's
showing Upper vs Lower? — needs a new ViewEvent: `EditLayerChanged`.
File a follow-up if not already covered by `KeyModeChanged` in 0035.
For this ticket, hard-bind controls to Upper and leave a TODO; 0045
ties up the layer toggle along with the Voice panel (which also
shows per-layer).

The rotary WaveformKnob is the visually distinctive piece — its
arc-arranged glyphs are what makes the faceplate look like a faceplate.
Port the geometry from `wave_points` in vxn-ui/src/lib.rs verbatim
(it's already coordinates-only).
