---
id: "0070"
title: Small helpers (clampVariant, pointerNorm) and tgRow miss in keysPanel
priority: low
created: 2026-06-01
epic: E014
---

## Summary

Three one-line fixes bundled because each on its own is too small
to warrant a ticket:

1. `clampVariant(plain, variants)` collapses the four occurrences
   of `Math.max(0, Math.min(variants.length - 1, Math.round(plain)))`.
2. `pointerNorm(ev, fader)` collapses the two occurrences of
   `Math.max(0, Math.min(1, 1 - (ev.clientY - r.top) / r.height))`.
   If 0068 lands first the helper lives inside `wireFaderDrag` and
   this part of 0070 is a no-op — close it then.
3. `keysPanel`'s mode list and edit list each inline the same
   `<div class="ctl-tg-box"></div><div class="ctl-tg-lbl">…</div>`
   markup that the existing `tgRow(label)` helper produces.

## Acceptance criteria

- [ ] Add `clampVariant(plain, variants)` to
      [panels.js](../../crates/vxn-ui-web/assets/panels.js):
      ```js
      // Plain → variant index clamp. Round to nearest, clamp to
      // [0, variants.length - 1]. The four enum-shaped primitives
      // (Switch, ButtonGroup, Dropdown, Wave-knob drag) all need
      // exactly this.
      function clampVariant(plain, variants) {
        return Math.max(0, Math.min(variants.length - 1, Math.round(plain)));
      }
      ```
- [ ] Replace the four sites: `makeWave`'s drag and update;
      `makeSwitch`'s update; `makeButtonGroup`'s update; `makeDropdown`'s
      update. Each becomes `clampVariant(plain, variants)` (or `variants`
      / `desc.variants` per local name).
- [ ] If 0068 has *not* landed: add `pointerNorm(ev, fader)` as a
      module-level helper and replace both call sites in `makeFader`
      and `makeDetuneLegato`. If 0068 *has* landed: confirm no bare
      `Math.max(0, Math.min(1, 1 - (ev.clientY` patterns remain
      outside `wireFaderDrag`, and skip this acceptance item.
- [ ] [panels.js](../../crates/vxn-ui-web/assets/panels.js)
      `keysPanel` — replace the two inline `row.innerHTML = '<div class="ctl-tg-box">…'`
      assignments inside `renderModeList` and `renderEditList` with
      `tgRow(label)`. The function returns a fresh `.ctl-tg-row`
      element; the caller adds the `'active'` class and the
      `pointerdown` listener as today.
- [ ] After: grep `'Math.max(0, Math.min(variants'` returns no
      hits; grep `'ctl-tg-box'` in `keysPanel`'s body shows the
      class only via the `tgRow()` helper, not inline markup.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

`tgRow` returns a fresh element each call — `keysPanel`'s render
functions clear the list container first (`innerHTML = ''`), then
append fresh rows, so they need the construct-on-demand shape
`tgRow` already provides.

If sequencing is awkward (0068 lands before this and `pointerNorm`
is already gone), just skip the pointer-norm item and note "no-op
post-0068" in the close-out.
