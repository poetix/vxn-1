---
id: "0064"
title: discrete(id, plain) helper collapses gesture-bracketed writes
priority: high
created: 2026-06-01
epic: E014
---

## Summary

Add `window.vxn.send.discrete(id, plain)` that posts the
`beginGesture → setParam → endGesture` triplet in one call.
Replace the 13 hand-rolled triplets across
[panels.js](../../crates/vxn-ui-web/assets/panels.js) and
[dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js) with the
helper. Wire-identical to the existing pattern; saves ~30 lines
and makes the "one click → one host edit" semantic explicit.

Depends on 0063 (typed senders).

## Acceptance criteria

- [ ] [crates/vxn-ui-web/assets/bridge.js](../../crates/vxn-ui-web/assets/bridge.js):
      add `discrete(id, plain)` as a method on `window.vxn.send`,
      defined immediately after `endGesture`:
      ```js
      // One-click discrete write. Brackets the set_param in a
      // begin/end gesture so the host records a single edit rather
      // than a zero-width gesture-less write some hosts drop.
      discrete(id, plain) {
        this.beginGesture(id);
        this.setParam(id, plain);
        this.endGesture(id);
      },
      ```
- [ ] Replace every site of the three-call pattern with one
      `window.vxn.send.discrete(id, plain)` call. The 13 sites
      (post-0063 grep is `send\.beginGesture`; pairs with `send.setParam`
      and `send.endGesture` immediately around them):
      - [panels.js](../../crates/vxn-ui-web/assets/panels.js) wave-knob
        glyph click (in `makeWave`, on the SVG `g.addEventListener('pointerdown', …)`
        branch).
      - `makeSwitch` row click in `makeSwitch`.
      - `makeButtonGroup` row click.
      - `makeDropdown` change.
      - `makeHeaderSwitch` pointerdown.
      - `makeDetuneLegato`: legato row click; double-click detune
        reset on `el`; Twin clamp inside `modeUpdate`.
      - [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
        generic `dblclick` reset inside `bindCell`.
- [ ] Grep for `send.beginGesture` afterwards: only `makeFader` and
      `makeDetuneLegato`'s fader (which need the gesture *open* across
      multiple `setParamNorm` / `setParam` calls during a drag) should
      remain. Every other use should have collapsed to `discrete`.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The Twin clamp inside `makeDetuneLegato::modeUpdate` is a clamp
write that happens in response to an automation echo — it's still
a discrete write (single value, no live drag). Use `discrete` there
too.

`makeWave`'s drag path (vertical-drag rotation) keeps the manual
`beginGesture` / `endGesture` because the gesture spans many
`setParam` calls. The glyph-click branch is the only one that
collapses.

Subtle: the glyph-click in `makeWave` posts `set_param` with the
*variant index* (not norm). `discrete` works because it just
forwards `plain` to `setParam`.
