---
id: "0069"
title: Extract attachValuePop for hover/drag popup lifecycle
priority: medium
created: 2026-06-01
epic: E014
---

## Summary

`makeFader`, `makeWave`, and `makeDetuneLegato` each maintain a
`hovered` / `dragging` pair and call `valuePop.show / update /
hide` at parallel points (pointerenter, pointerdown, pointermove,
pointerup, pointerleave, ParamChanged update). Extract the
lifecycle into a small `attachValuePop` helper so the three
control primitives just say "popup follows this drag, label is X".

## Acceptance criteria

- [ ] New helper in
      [panels.js](../../crates/vxn-ui-web/assets/panels.js), near
      `valuePop` (which lives in `bridge.js` but is the global the
      helper drives):
      ```js
      // Attaches the floating value popup's lifecycle to a control.
      // `getLabel()` returns the current display string (the popup
      // shows + updates against it). The host control calls
      // `popup.markGrabbed(ev)` on pointerdown (anchor + show),
      // `popup.markReleased()` on pointerup (hide unless hovered),
      // `popup.refresh()` on ParamChanged echo (update text iff
      // hovered or dragging). `host` is the drag state from
      // `wireFaderDrag` (or any object with `isHovered()`,
      // `isDragging()`).
      function attachValuePop(host, getLabel) {
        return {
          markEntered(ev) {
            if (host.isDragging()) return;
            valuePop.show(getLabel(), ev.clientX, ev.clientY);
          },
          markLeft() {
            if (!host.isDragging()) valuePop.hide();
          },
          markGrabbed(ev) {
            valuePop.show(getLabel(), ev.clientX, ev.clientY);
          },
          markReleased() {
            if (!host.isHovered()) valuePop.hide();
          },
          refresh() {
            if (host.isHovered() || host.isDragging()) {
              valuePop.update(getLabel());
            }
          },
        };
      }
      ```
- [ ] `makeFader` uses `attachValuePop(drag, () => lastDisplay)`
      where `drag` is the return value of `wireFaderDrag` (post-0068).
      The fader's `wireFaderDrag` callbacks invoke
      `pop.markEntered(ev)`, `pop.markLeft()`, `pop.markGrabbed(ev)`,
      `pop.markReleased()`. The `update(plain, norm, display)`
      ParamChanged hook updates `lastDisplay` then calls
      `pop.refresh()`.
- [ ] `makeWave` uses `attachValuePop` similarly. The drag-state
      adapter needs `isHovered()`/`isDragging()` getters synthesized
      from `makeWave`'s local `hovered` / `dragging` vars — since
      `makeWave` keeps its own drag handlers (per 0068's notes),
      pass an inline shim `{ isHovered: () => hovered, isDragging: () => dragging }`.
- [ ] `makeDetuneLegato`'s detune fader uses `attachValuePop`. Its
      `getLabel()` is `() => lastDetunePlain.toFixed(1) + ' ct'`
      (or `display || …` per the existing `detuneUpdate`).
- [ ] All `valuePop.show` / `valuePop.update` / `valuePop.hide`
      call sites in `makeFader`, `makeWave`, `makeDetuneLegato`
      disappear; only the helper touches `valuePop`.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

`valuePop` itself stays in
[bridge.js](../../crates/vxn-ui-web/assets/bridge.js) — it's a
shared singleton DOM element. `attachValuePop` is the per-control
adapter.

If 0068 hasn't landed yet, this ticket still applies — `attachValuePop`
takes any object with `isHovered()` / `isDragging()` getters,
including a hand-rolled shim over module-locals. After 0068 the
`wireFaderDrag` return value is the canonical host.

The naming `markEntered` / `markLeft` / `markGrabbed` / `markReleased`
mirrors the verbs used in the existing drag scaffolding (entered =
pointerenter, grabbed = pointerdown). `refresh` is for the
ParamChanged-echo update.
