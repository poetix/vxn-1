---
id: "0083"
title: `paintFader` helper collapses thumb-from-norm math
priority: low
created: 2026-06-01
epic: E016
---

## Summary

The 0075 audit's finding N4: `setThumb` in `makeFader`
([panels.js:351–364](../../crates/vxn-ui-web/assets/panels.js#L351-L364))
and `setThumbFromPlain` in `makeDetuneLegato`
([panels.js:839–846](../../crates/vxn-ui-web/assets/panels.js#L839-L846))
share the same `halfThumb + (1 - n) * travel` thumb-positioning
formula. Extract `paintFader(fader, thumb, norm)`. The composite's
plain-to-norm mapping (Twin-aware ceiling) lives in `currentTop()`
and stays in the composite — only the norm-to-pixel pass moves.

## Acceptance criteria

- [ ] [panels.js](../../crates/vxn-ui-web/assets/panels.js)
      add `paintFader(fader, thumb, norm)`:
      ```js
      // Paint a vertical fader's thumb at a [0, 1] norm. Clamps
      // the thumb's centre so its bounding box stays inside the
      // fader element regardless of --fader-h / --thumb-h tweaks.
      // Also sets the --fader-norm custom property for any
      // dependent CSS (track fill colour, etc).
      function paintFader(fader, thumb, norm) {
        const halfThumb = thumb.offsetHeight / 2;
        const travel = fader.clientHeight - thumb.offsetHeight;
        const n = Math.max(0, Math.min(1, norm));
        thumb.style.top = (halfThumb + (1 - n) * travel) + 'px';
        fader.style.setProperty('--fader-norm', n);
      }
      ```
- [ ] `makeFader`'s local `setThumb` becomes a closure that calls
      `paintFader(fader, thumb, norm)` (or its body inlines the
      call directly — pick whichever reads cleaner; the indirection
      may not be worth keeping once the helper exists).
- [ ] `makeDetuneLegato`'s `setThumbFromPlain` becomes:
      ```js
      function setThumbFromPlain(plain) {
        const top = currentTop();
        paintFader(fader, thumb, top > 0 ? plain / top : 0);
      }
      ```
- [ ] `grep "halfThumb + (1 - n)"` in
      [crates/vxn-ui-web/assets/](../../crates/vxn-ui-web/assets/)
      returns exactly one hit (inside `paintFader`).
- [ ] [crates/vxn-ui-web/assets/__tests__/paint-fader.test.js](../../crates/vxn-ui-web/assets/__tests__/paint-fader.test.js)
      covers:
      - `norm = 0` lands the thumb's centre at `fader.clientHeight
        - halfThumb` (bottom).
      - `norm = 1` lands the thumb's centre at `halfThumb` (top).
      - `norm = 0.5` lands the thumb's centre at the midpoint.
      - `norm < 0` and `norm > 1` clamp correctly.
      - `--fader-norm` custom property set to the clamped norm.
      - Uses jsdom-mounted fader / thumb elements with stubbed
        `offsetHeight` / `clientHeight` (jsdom doesn't compute
        layout; `Object.defineProperty(el, 'clientHeight', { value: 100 })`
        before the call).
- [ ] Manual smoke (ask first): every fader thumb still moves
      smoothly under drag and DAW automation; detune fader still
      respects Twin's 20-ct ceiling visually.
- [ ] `npm test` and `cargo test -p vxn-ui-web` pass.

## Notes

`currentTop()` stays in the composite — it's the Twin-aware
descriptor ceiling and depends on `lastModePlain`, both of which
are composite-internal. `paintFader` is pure geometry.

If 0085 (`tgRow` helper) lands first this ticket is independent.
If 0082 (`wireDrag`) lands first the `setThumb` callsites inside
`onDown` / `onMove` change but the helper's contract is unchanged.
