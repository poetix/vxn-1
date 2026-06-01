---
id: "0081"
title: Inline trims — dead `_browserOpen`, redundant rename ternary
priority: low
created: 2026-06-01
epic: E016
---

## Summary

Two one-line cleanups from the 0075 close-out audit (findings N8 +
N9). Bundled because each on its own is too small to ticket.

1. **N8** — [browser.js:385](../../crates/vxn-ui-web/assets/browser.js#L385)
   writes `window.vxn._browserOpen = isOpen` from `setOpen`. Nothing
   reads it. Dead code left over from the pre-`onOpenChange` design.
2. **N9** — [browser.js:429](../../crates/vxn-ui-web/assets/browser.js#L429)
   computes `const renameLabel = target.kind === 'preset' ? target.name : target.name`.
   Same expression both branches; collapses to `const renameLabel = target.name`.

## Acceptance criteria

- [ ] [browser.js:385](../../crates/vxn-ui-web/assets/browser.js#L385)
      line removed.
- [ ] [browser.js:429](../../crates/vxn-ui-web/assets/browser.js#L429)
      ternary reduced to the direct assignment.
- [ ] `grep _browserOpen` in
      [crates/vxn-ui-web/assets/](../../crates/vxn-ui-web/assets/)
      returns zero hits.
- [ ] Manual smoke (ask first): preset bar Browse button still
      toggles its `.active` class on click and on ESC / backdrop
      dismissal (proves `onOpenChange` is the only contract);
      rename a user preset and a user folder (proves the simplified
      `renameLabel` initialises the popup correctly).
- [ ] `npm test` passes; `cargo test -p vxn-ui-web` passes.

## Notes

No test surface added — these are inline trims, not new behaviour.
The existing 0080 `browser-invariants` test for `onOpenChange`
covers the relevant contract.

If E015 hasn't landed yet, skip the `npm test` assertion and let
the substring suite alone gate the change. Either way the diff is
two lines.
