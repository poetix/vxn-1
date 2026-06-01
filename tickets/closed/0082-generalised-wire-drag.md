---
id: "0082"
title: Generalised `wireDrag` covers fader and wave
priority: medium
created: 2026-06-01
epic: E016
---

## Summary

The 0075 audit's finding N2: `makeWave` ([panels.js:566–595](../../crates/vxn-ui-web/assets/panels.js#L566-L595))
re-implements the pointer-capture / drag / release scaffolding that
`wireFaderDrag` ([panels.js:265](../../crates/vxn-ui-web/assets/panels.js#L265))
already encapsulates. The wave knob's drag is rotational rather than
linear-norm, so the existing helper doesn't fit verbatim — but
parameterising the pointer-to-value map lets one helper cover both.

Define `wireDrag(el, { pointerToValue }, callbacks)` where
`pointerToValue(ev, downCtx)` returns whatever the caller cares
about (a `[0, 1]` norm for the fader; a pixel-delta-derived variant
index for the wave knob). The existing `wireFaderDrag` becomes a
thin wrapper that supplies the norm computation. Wave knob calls
`wireDrag` directly with its own delta-based map.

## Acceptance criteria

- [ ] [panels.js](../../crates/vxn-ui-web/assets/panels.js)
      add `wireDrag(el, { pointerToValue, downContext }, {
      onEnter, onDown, onMove, onUp, onLeave })`:
      - `downContext(ev)` runs once on pointerdown; its return
        value is stashed and passed as the second arg to every
        subsequent `pointerToValue(ev, ctx)` call. Lets the wave
        knob capture `dragStartY` + `dragStartValue` cleanly.
      - `pointerToValue(ev, ctx)` runs on `pointerdown` (for the
        initial `onDown` arg) and `pointermove`.
      - The hover / drag / pointer-capture / dragging-class
        bookkeeping matches today's `wireFaderDrag`.
      - Returns `{ isDragging, isHovered }` getters.
- [ ] Refactor `wireFaderDrag` to be a wrapper that calls
      `wireDrag` with `pointerToValue: (ev) => clampedNorm(ev, el)`
      and no `downContext`. Existing callers (`makeFader`,
      `makeDetuneLegato`) untouched.
- [ ] [panels.js `makeWave`](../../crates/vxn-ui-web/assets/panels.js#L446)
      drops its inline `pointerdown` / `pointermove` /
      `pointerup` / `pointercancel` listeners. Drag goes through
      `wireDrag` with:
      ```js
      wireDrag(svg, {
        downContext: (ev) => ({ y0: ev.clientY, v0: value }),
        pointerToValue: (ev, ctx) =>
          clampVariant(ctx.v0 + (ctx.y0 - ev.clientY) / PIXELS_PER_DETENT,
                       variants),
      }, {
        onEnter: …,
        onDown: (ev, v) => { window.vxn.send.beginGesture(id); … },
        onMove: (_ev, v) => { if (v !== value) window.vxn.send.setParam(id, v); },
        onUp: …,
        onLeave: …,
      });
      ```
      Glyph clicks (`pointerdown` + `stopPropagation`) keep their
      own listener — they're not drag.
- [ ] `grep setPointerCapture` in
      [crates/vxn-ui-web/assets/](../../crates/vxn-ui-web/assets/)
      returns exactly one hit (inside `wireDrag`).
- [ ] [crates/vxn-ui-web/assets/__tests__/wire-drag.test.js](../../crates/vxn-ui-web/assets/__tests__/wire-drag.test.js)
      covers:
      - The norm-based `pointerToValue` path (today's
        `wireFaderDrag` semantics — half of these may already exist
        in 0079; update names, don't duplicate).
      - The delta-based path: `downContext` stashes start state;
        `pointerToValue` receives it on every move; the helper
        doesn't otherwise interpret the return value.
      - Hover-during-drag suppression is identical to the previous
        helper's contract.
- [ ] Manual smoke (ask first): wave knob drag still rotates;
      glyph clicks still select; fader drag unchanged.
- [ ] `npm test` and `cargo test -p vxn-ui-web` pass.

## Notes

The wave knob's old code captures `dragStartY` and `dragStartValue`
in closure-scoped `let`s. With `downContext` those become an
explicit object returned to the helper, which closes over them
internally. Less stateful in `makeWave`'s body; the helper carries
the state.

The `displayedAngle` indicator animation stays in `makeWave` —
it's the visual side effect of a value change, not part of the
drag protocol. `applyValue(v, display)` continues to set the
transform; `wireDrag` doesn't know about it.

If 0079 has landed, that ticket's tests need a rename pass:
either reference `wireDrag` directly or keep `wireFaderDrag` as
the thin wrapper and have the tests target it. Pick whichever
makes the diff smaller.
