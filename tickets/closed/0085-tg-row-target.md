---
id: "0085"
title: `tgRow` accepts target; detune-legato uses it
priority: low
created: 2026-06-01
epic: E016
---

## Summary

The 0075 audit's finding N3: `makeDetuneLegato` open-codes the
tg-row markup at [panels.js:815–817](../../crates/vxn-ui-web/assets/panels.js#L815-L817)
because the composite needs the row stamped *into* an existing
`.ctl-detune-legato` container with a fixed "LEGATO" label.
`tgRow(label)` today returns a fresh `<div>` chain — the composite
can't use it because it already has a target.

Parameterise: `tgRow(label, { mount? })` either mounts into a
caller-supplied target (and returns the same target) or returns a
fresh row as today. Composite uses the mount form; existing callers
unchanged.

## Acceptance criteria

- [ ] [panels.js `tgRow`](../../crates/vxn-ui-web/assets/panels.js#L636)
      gains a second arg:
      ```js
      function tgRow(name, opts) {
        const target = (opts && opts.mount) || document.createElement('div');
        if (!opts || !opts.mount) target.className = 'ctl-tg-row';
        target.innerHTML =
          '<div class="ctl-tg-box"></div>' +
          '<div class="ctl-tg-lbl">' + name.toUpperCase() + '</div>';
        return target;
      }
      ```
- [ ] [`makeDetuneLegato`](../../crates/vxn-ui-web/assets/panels.js#L815)
      drops the inline `legatoRow.innerHTML = …` assignment and
      replaces it with `tgRow('LEGATO', { mount: legatoRow })`.
- [ ] `grep "ctl-tg-box"` in
      [crates/vxn-ui-web/assets/](../../crates/vxn-ui-web/assets/)
      returns exactly one hit (inside `tgRow`).
- [ ] [crates/vxn-ui-web/assets/__tests__/tg-row.test.js](../../crates/vxn-ui-web/assets/__tests__/tg-row.test.js)
      covers:
      - Standalone form: returns a new `.ctl-tg-row` with `.ctl-tg-box`
        and `.ctl-tg-lbl` children; label is uppercased.
      - Mount form: returns the supplied target, mounts the inner
        markup, does *not* set `.ctl-tg-row` (caller's class
        applies).
      - Two separate calls produce independent DOM (no shared
        innerHTML reference).
- [ ] Manual smoke (ask first): detune-legato composite still
      shows the LEGATO toggle; clicking it still flips the legato
      param; existing tg-row callers (Keys panel, switch,
      button-group) unchanged.
- [ ] `npm test` and `cargo test -p vxn-ui-web` pass.

## Notes

The composite's `.ctl-detune-legato` container is created as part
of the composite's main `innerHTML` template — `tgRow`'s mount
form just fills it. The composite's container also has the
`ctl-tg-row` class added by the template, which is why the helper
must not re-add it on the mount path (would be a no-op but reads
worse).

The opts object is intentional — a second-positional `target`
argument would conflict with future "tooltip text" or "modifier
class" options. `{ mount }` is unambiguous.
