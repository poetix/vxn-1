---
id: "0073"
title: Move browserPanel to its own browser.js module
priority: medium
created: 2026-06-01
epic: E014
---

## Summary

`browserPanel` is ~770 lines inside
[panels.js](../../crates/vxn-ui-web/assets/panels.js), doing six
distinct concerns (corpus model, render, search, context menu,
modal, DnD). Move it to a new file
`crates/vxn-ui-web/assets/browser.js` with its own
`__BROWSER_JS__` placeholder, spliced after `__BRIDGE_JS__` and
before the rest of `panels.js`. The shape after split: bridge → browser →
panels → dispatch.

This isn't sub-splitting the browser further (one file is the
right granularity for one logical concern); it's removing the
biggest single block from `panels.js` so the controls / keys
panel / preset bar are the easier-to-scan ~900 lines they actually
should be.

## Acceptance criteria

- [ ] Create [crates/vxn-ui-web/assets/browser.js](../../crates/vxn-ui-web/assets/browser.js).
      Move the `// ─── Preset browser panel ───` block, including the
      replacement `window.__vxn.applyPresetCorpus = (snap) => browserPanel.setCorpus(snap);`
      drain + the `if (_earlyPresetCorpus) {…}` block immediately
      after the IIFE.
- [ ] [crates/vxn-ui-web/assets/panels.js](../../crates/vxn-ui-web/assets/panels.js):
      the browser block (currently the first ~770 lines) is gone;
      the file starts with the preset bar IIFE.
- [ ] [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs):
      add the include + splice:
      ```rust
      /// Preset browser panel — corpus model, folder/preset rendering,
      /// search, context menu, modal confirms (delete + save-as), DnD.
      /// Splices between bridge and the rest of panels because the bar
      /// IIFE (`const presetBar = …`) references `browserPanel`.
      const BROWSER_JS: &str = include_str!("../assets/browser.js");
      ```
      and
      ```rust
      // build_faceplate_html(), in the JS splice section:
      .replace("__BROWSER_JS__", BROWSER_JS)
      ```
      Place the `__BROWSER_JS__` line **between** `__BRIDGE_JS__`
      and `__PANELS_JS__`, both in the doc-comment description and
      the `.replace` chain.
- [ ] [crates/vxn-ui-web/assets/faceplate.html](../../crates/vxn-ui-web/assets/faceplate.html):
      insert `__BROWSER_JS__` between `__BRIDGE_JS__` and
      `__PANELS_JS__` inside the `<script>` block.
- [ ] Substring tests in
      [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs)
      keep passing — `assembled()` includes the browser content
      regardless of which file it came from. No test change needed.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

`presetBar`'s IIFE (in `panels.js`) calls `browserPanel.setOpen(…)`,
`browserPanel.isOpen()`, `browserPanel.openSaveAs(…)`, and
`browserPanel.onOpenChange(…)`. These resolve because `browser.js`
splices before `panels.js` — `const browserPanel = (() => { … })()`
runs first, populating the lexical scope.

`dispatch.js` references `browserPanel.setCurrentSource`,
`browserPanel.followPath`. Same scope rule applies — splice
order: bridge → browser → panels → dispatch.

The substring test `faceplate_browser_panel_wired` and friends
check assembled HTML, not file boundaries, so they keep working.
A bored author *could* add a test that the browser content is
in fact in `browser.js` (e.g. `assert!(BROWSER_JS.contains("const browserPanel"))`);
optional but cheap.

If 0074 (unbundle modals) lands after this, the modal split
happens inside `browser.js` — both modal flows live there.
