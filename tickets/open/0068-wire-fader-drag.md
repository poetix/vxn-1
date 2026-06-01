---
id: "0068"
title: Extract wireFaderDrag for shared pointer-drag scaffolding
priority: medium
created: 2026-06-01
epic: E014
---

## Summary

`makeFader` and `makeDetuneLegato` both implement the same vertical-
drag pattern against a `.ctl-fader` element: pointerdown / move /
up / cancel / enter / leave, pointer capture, the `dragging` class,
hover-vs-drag state, and the per-event `pointerNorm(ev)` calc.
~70 lines duplicated. Extract a `wireFaderDrag(fader, callbacks)`
helper in [panels.js](../../crates/vxn-ui-web/assets/panels.js).

## Acceptance criteria

- [ ] New helper near the top of the "Control primitives" section
      of [panels.js](../../crates/vxn-ui-web/assets/panels.js):
      ```js
      // Wires the vertical-drag protocol shared by every fader-shaped
      // control. Callbacks fire in order:
      //   onEnter(ev)             — hover begins (not during drag)
      //   onDown(ev, norm)        — pointer down, drag starts. `norm` is
      //                             the pointer's [0, 1] vertical position
      //                             on the fader at down-time.
      //   onMove(ev, norm)        — drag-time move. Fires only while
      //                             dragging; ignored otherwise.
      //   onUp(ev)                — drag ends (pointerup or cancel).
      //   onLeave()               — hover ends (not during drag).
      // Returns { isDragging, isHovered } getters for callers whose
      // ParamChanged echoes need to know whether to update the popup.
      function wireFaderDrag(fader, { onEnter, onDown, onMove, onUp, onLeave }) {
        let dragging = false;
        let hovered  = false;
        const norm = (ev) => {
          const r = fader.getBoundingClientRect();
          return Math.max(0, Math.min(1, 1 - (ev.clientY - r.top) / r.height));
        };
        fader.addEventListener('pointerenter', (ev) => {
          if (dragging) return;
          hovered = true;
          if (onEnter) onEnter(ev);
        });
        fader.addEventListener('pointerleave', () => {
          hovered = false;
          if (!dragging && onLeave) onLeave();
        });
        fader.addEventListener('pointerdown', (ev) => {
          ev.preventDefault();
          dragging = true;
          fader.classList.add('dragging');
          fader.setPointerCapture(ev.pointerId);
          if (onDown) onDown(ev, norm(ev));
        });
        fader.addEventListener('pointermove', (ev) => {
          if (!dragging || !onMove) return;
          onMove(ev, norm(ev));
        });
        const end = (ev) => {
          if (!dragging) return;
          dragging = false;
          fader.classList.remove('dragging');
          try { fader.releasePointerCapture(ev.pointerId); } catch (e) {}
          if (onUp) onUp(ev);
          if (!hovered && onLeave) onLeave();
        };
        fader.addEventListener('pointerup', end);
        fader.addEventListener('pointercancel', end);
        return {
          isDragging: () => dragging,
          isHovered:  () => hovered,
        };
      }
      ```
- [ ] `makeFader` rewrites to call `wireFaderDrag` and keep only
      the norm → IPC mapping (`send.beginGesture`, `send.setParamNorm`,
      `send.endGesture`) plus the `setThumb` / `valuePop` calls
      inside the callbacks. The `pointerNorm`, `dragging`, `hovered`,
      and all six listener bindings disappear. ~40 lines lighter.
- [ ] `makeDetuneLegato`'s detune-fader half rewrites the same way.
      The IPC maps norm → plain via `currentTop()`; place that map
      inside the callbacks. ~30 lines lighter.
- [ ] `makeDetuneLegato`'s `dblclick` on the cell root stays as-is
      (it's a single-shot, not a drag).
- [ ] Substring tests update if any check for the now-gone literals
      (e.g. `pointerNorm` if used as an asserted symbol). Run the
      suite; fix what fails.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The `dblclick` reset hooked on the cell `el` in `bindCell` is
separate from this — it's not a drag, it's a one-shot. Leave it
as-is (post-0064 it uses `send.discrete`).

The popup show / update / hide calls inside `makeFader` /
`makeDetuneLegato` are 0069's territory — this ticket only
extracts the *drag* lifecycle. Either ticket can land first; both
together compose the final shape.

`makeWave` has a similar but *not identical* drag shape — it tracks
delta from `dragStartY` rather than absolute pointer position
within the element. Don't try to fold it into `wireFaderDrag` —
the abstraction stops being useful when the value mapping diverges
that much. Leave `makeWave`'s drag handlers in place.
