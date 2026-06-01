---
id: "0075"
title: Re-review faceplate JS against the original findings
priority: high
created: 2026-06-01
epic: E014
---

## Summary

After 0063–0074 close, re-walk
[bridge.js](../../crates/vxn-ui-web/assets/bridge.js),
[browser.js](../../crates/vxn-ui-web/assets/browser.js) (new in 0073),
[panels.js](../../crates/vxn-ui-web/assets/panels.js), and
[dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js).
Confirm every 2026-06-01 review finding is addressed and that the
cleanup didn't introduce new instances of the same pattern. This is
the closing audit on E014, not new design work.

## Acceptance criteria

- [ ] **Per-finding sweep.** For each of the 16 original findings,
      confirm in writing (one line per finding in the close-out
      comment) that it is resolved:

      | # | Finding | Verifier |
      |---|---------|----------|
      | 1 | Gesture-bracketed writes (13 sites) | grep `beginGesture` — only `makeFader` + `makeDetuneLegato`'s fader drag remain |
      | 2 | Fader drag scaffolding duplicated | only `wireFaderDrag` callers — no inline drag handler chains |
      | 3 | Hover/drag valuePop lifecycle | only `attachValuePop` callers — no inline `valuePop.show / update / hide` outside the helper |
      | 4 | Variant clamp 4× | grep `Math.max(0, Math.min(variants.length` returns 0 hits outside `clampVariant` |
      | 5 | Pointer-norm 2× | grep `1 - (ev.clientY - r.top) / r.height` returns 0 hits outside `wireFaderDrag` |
      | 6 | tgRow miss in keysPanel | grep `ctl-tg-box` in keysPanel returns only via `tgRow()` |
      | 7 | Opcode strings scattered | grep `op: '` returns 0 hits — only typed sender names |
      | 8 | Variant-by-name lookups | grep `variants.indexOf(` returns 0 hits outside `variantIdx` |
      | 9 | Dim rules split between dispatch + DIM_RULES | dispatch has one `applyDimRulesFor(ev.id, ev.plain)` branch — no `ev.id === FREE_RUN_ID` / `ev.id === FILTER_MODE_ID` |
      | 10 | LAYERED_CELLS + STATIC_CELLS | one `model.cells` array; entries carry `layered: bool` |
      | 11 | Dispatch state as globals | grep `const ` and `let ` at module-level in `dispatch.js` returns only function defs + `model` decl |
      | 12 | browserPanel ~770 lines in panels.js | `browser.js` exists; `panels.js` does not contain `const browserPanel` |
      | 13 | openModal body-polymorphism + extendActions | grep `extendActions` returns 0 hits; `openConfirmModal` + `openSaveAsModal` distinct |
      | 14 | Magic numbers (3000ms, 120ms) | `STATUS_PILL_FLASH_MS` + `KNOB_INDICATOR_TRANSITION_MS` defined at module scope |
      | 15 | TWIN_TOP_CT buried | `TWIN_TOP_CT` defined at module scope alongside `PIXELS_PER_DETENT` |
      | 16 | (wave glyphs / KEY_MODE_NAMES already good) | no change; confirm still good |
- [ ] **No regression sweep.** Walk the three (now four) files end
      to end looking for *new* instances of the same patterns. Specific
      things to look for:
      - Bypass of typed senders — any new code path that calls
        `window.ipc.postMessage` directly or constructs a `{op: …}`
        object outside `bridge.js`.
      - Bypass of `discrete` — any new `beginGesture / setParam /
        endGesture` triplet outside `bridge.js`'s `discrete` helper.
      - State leaks — any new module-level `let` / `const` in
        `dispatch.js` (or `browser.js`) outside the `model` object.
      - Helper bypass — bare `Math.max(0, Math.min(variants.length …`
        / `variants.indexOf('…')` / `1 - (ev.clientY - r.top) / r.height`
        anywhere. Should be 0 hits in each grep.
      - Magic numbers — any new numeric literal in a `setTimeout`,
        a CSS transition string, or a clamp ceiling that isn't
        named at module scope.
- [ ] **Fresh review pass.** Re-read each file in full, looking
      for *new* findings that didn't surface in the 2026-06-01
      review — patterns that emerged from the cleanup, or that
      were obscured by the original mess. Document any genuine
      finding (don't manufacture them) as either:
      - A follow-up ticket under E014 (if it fits the cleanup
        epic).
      - A new note in the close-out comment, with a recommendation
        for a future epic if E014 is the wrong home.
- [ ] **Smoke.** Per the `ask-before-screen-capture` rule, ask
      first: run the plugin in a host. Confirm:
      - Every panel renders.
      - Every fader drags + commits via gestures (host records as
        one edit, not many).
      - Every wave knob rotates + glyph-click selects directly.
      - Every switch / buttongroup / dropdown / header-switch
        flips state.
      - Detune-legato: Twin clamps detune to 20 ct on entering
        Twin; Legato dims outside Mono modes.
      - Layer flip rebinds all per-patch panels.
      - Key-mode flip shows/hides the split-row + edit-toggle
        correctly.
      - Preset bar prev/next/Browse/Save As all work; status pill
        flashes on load warnings.
      - Browser panel: search, click-load, context menu rename /
        delete / move, "+ New" folder, DnD, modal confirms.
      - Text-input popup commits and cancels.
- [ ] `cargo test -p vxn-ui-web` passes — the full substring
      suite, including any new assertions added by the per-finding
      tickets.

## Notes

This is a written audit, not a refactor pass. If the audit
surfaces a real new finding the author *can* fix it inline
(small, obvious cleanups) — but anything bigger goes to a
follow-up ticket so the audit stays an audit.

The grep checks listed above are sanity probes, not exhaustive
proofs. The substantive part of this ticket is the close-out
comment that explicitly names each of the 16 findings and where
the resolution lives (which sibling ticket, which line in which
file). That comment is what future-you reads when wondering
whether E014 actually landed what it said it would.

If a finding turned out to be a non-issue once attempted — e.g.
the `attachValuePop` helper read worse than the inline form once
written — record that too. "We tried and rolled back because X"
is a valid resolution and worth preserving.
