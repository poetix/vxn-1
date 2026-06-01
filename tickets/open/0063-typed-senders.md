---
id: "0063"
title: Typed sender API for IPC opcodes
priority: high
created: 2026-06-01
epic: E014
---

## Summary

Replace the ~30 inline `window.vxn.send({op: 'set_param', id, plain})`
calls scattered across [panels.js](../../crates/vxn-ui-web/assets/panels.js)
and [bridge.js](../../crates/vxn-ui-web/assets/bridge.js) with a typed
sender namespace: `window.vxn.send.setParam(id, plain)`,
`window.vxn.send.beginGesture(id)`, etc. The wire shape stays
byte-identical — the senders build the same JSON. The Rust-side
parser in [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs)
does not change.

This is the foundation for 0064 (`discrete()` helper) — the helper
calls `send.beginGesture` / `send.setParam` / `send.endGesture`
once and every site that needs the bracketed triplet uses it.

## Acceptance criteria

- [ ] [crates/vxn-ui-web/assets/bridge.js](../../crates/vxn-ui-web/assets/bridge.js)
      `window.vxn.send` becomes an object with one method per
      opcode the page emits. Names map opcode → camelCase:
      `set_param` → `setParam`, `set_param_norm` → `setParamNorm`,
      `begin_gesture` → `beginGesture`, `end_gesture` → `endGesture`,
      `reset_layer` → `resetLayer`, `load_factory` → `loadFactory`,
      `load_user` → `loadUser`, `rename_preset` → `renamePreset`,
      `delete_preset` → `deletePreset`, `move_preset` → `movePreset`,
      `rename_folder` → `renameFolder`, `delete_folder` → `deleteFolder`,
      `new_folder` → `newFolder`, `step_preset` → `stepPreset`,
      `save_preset` → `savePreset`, `set_key_mode` → `setKeyMode`,
      `set_split_point` → `setSplitPoint`, `set_edit_layer` → `setEditLayer`,
      `request_text_input` → `requestTextInput` (used internally by
      `promptText`), `ready` → `ready`.
- [ ] Each sender wraps a single low-level `_post(msg)` that posts the
      JSON; `_post` is private (underscore prefix is convention only —
      it's still on `window.vxn.send` until JS gets real visibility).
- [ ] Every existing `window.vxn.send({op: '...', ...})` call site
      across [panels.js](../../crates/vxn-ui-web/assets/panels.js)
      and [bridge.js](../../crates/vxn-ui-web/assets/bridge.js) and
      [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
      switches to the typed call. No remaining `{op: '...'}` literals
      outside `bridge.js`.
- [ ] `_textInputCallbacks` plumbing in `bridge.js` stays; only its
      send call switches to `window.vxn.send.requestTextInput(...)`.
- [ ] Substring tests in
      [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs)
      that assert on `op: '...'` literals update to the new camelCase
      method names — search for `"op: '"` in the test file and
      replace each with the matching `.send.<method>(` assertion
      (the wire shape is verified by the Rust-side parser tests,
      which already check the JSON-on-the-wire shape).
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The wire format stays JSON; the senders are a thin façade. A future
move to a binary or strongly-typed wire is out of scope.

`_post` is the one place that constructs the `{op, …}` object —
adding a debug `console.log` or batching is a one-line change after
this. (Don't do that here; just keep the door open.)

The typed-sender call sites read better and survive an opcode
rename (rename the method; the wire string is one place). Catches
typos statically at the JS level too (`send.setParm(…)` fails fast
with `undefined is not a function`, where a string typo silently
sends a misnamed op the controller drops).

`subdivisions` / `patchCount` / `params` stay on `window.vxn`
directly — they're not senders. `promptText` keeps its current
shape (it captures the callback and posts `requestTextInput`
internally).
