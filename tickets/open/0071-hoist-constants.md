---
id: "0071"
title: Hoist magic numbers and TWIN_TOP_CT to a constants block
priority: low
created: 2026-06-01
epic: E014
---

## Summary

Three numeric constants live as inline literals inside their
respective IIFEs / closures:

- Status-pill flash duration `3000` ms — inside `statusPill` in
  [bridge.js](../../crates/vxn-ui-web/assets/bridge.js).
- Knob indicator transition `'transform 120ms ease-out'` — inside
  `makeWave` in [panels.js](../../crates/vxn-ui-web/assets/panels.js).
- Twin detune ceiling `20.0` (cents) — inside `makeDetuneLegato`
  in [panels.js](../../crates/vxn-ui-web/assets/panels.js).

Hoist each to a named constant next to the other domain constants
that already sit at the top of the file (`PIXELS_PER_DETENT`,
`KEYS_DEFAULT_SPLIT`, `KEY_MODE_NAMES`, `WAVE_GLYPHS`).
`TWIN_TOP_CT` specifically mirrors `vxn_ui_vizia::TWIN_DETUNE_CT`
in the (now-retired) vizia editor — surfacing it next to
`PIXELS_PER_DETENT` matches what the constant *is*.

## Acceptance criteria

- [ ] [bridge.js](../../crates/vxn-ui-web/assets/bridge.js): add
      `const STATUS_PILL_FLASH_MS = 3000;` near the top of the
      file (after the bridge bootstrap, before `valuePop` /
      `statusPill`). Replace the `3000` inside `statusPill`'s
      `setTimeout` with the constant.
- [ ] [panels.js](../../crates/vxn-ui-web/assets/panels.js):
      somewhere alongside `PIXELS_PER_DETENT` (already hoisted),
      add:
      ```js
      // Smoothing transition on the wave-knob indicator. Long enough
      // that automation moves don't strobe between detents; short
      // enough that drag still feels responsive.
      const KNOB_INDICATOR_TRANSITION_MS = 120;

      // Detune ceiling in Twin assign mode (cents). Twin's "useful"
      // range is purely a view convention — the engine doesn't
      // enforce it, so the editor that surfaces the mode is the one
      // that has to clamp. Mirrors vxn_ui_vizia::TWIN_DETUNE_CT
      // (retired in 0054 but the value is still load-bearing).
      const TWIN_TOP_CT = 20.0;
      ```
- [ ] Replace `'transform 120ms ease-out'` inside `makeWave` with
      `\`transform \${KNOB_INDICATOR_TRANSITION_MS}ms ease-out\``
      (template literal — direct number splice).
- [ ] Replace the function-local `const TWIN_TOP_CT = 20.0;` inside
      `makeDetuneLegato` with a reference to the module-level
      constant (delete the inner declaration).
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The 3-second flash is the only magic number in `statusPill`; the
240ms CSS transition is in the stylesheet (out of scope here).

The knob transition was empirically tuned; 120ms is the actual
preferred value. The constant name is descriptive but the value
is calibrated taste, not engineering — don't tweak it under this
ticket.

This ticket is trivial. The reason it's worth doing as its own
ticket is the audit value: a future reader scanning for "what
constants does this editor enforce" sees them together. Burying
domain caps inside closures means a comment somewhere else has to
explain that they exist.
