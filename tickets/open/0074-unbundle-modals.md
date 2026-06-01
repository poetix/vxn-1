---
id: "0074"
title: Unbundle Save-As modal from delete-confirm
priority: low
created: 2026-06-01
epic: E014
---

## Summary

`openModal({ title, body, confirmLabel, danger, onConfirm,
extendActions })` exists in
[panels.js](../../crates/vxn-ui-web/assets/panels.js) (will be in
`browser.js` post-0073) as a shared primitive for two callers:
delete-confirm (string body, danger flag, no extendActions) and
Save-As (function body, no danger, extendActions returns a
`setEnabled` hook to gate the Save button).

The two callers share so little — body shape, action set, enable-gating
logic — that the shared abstraction costs more than it saves. The
`body: fn | string` polymorphism + the awkward `extendActions`
callback that hands a `setEnabled` reference back are both pure
artefacts of the merge. Split into two purpose-built functions.

## Acceptance criteria

- [ ] In `browser.js` (or `panels.js` if 0073 hasn't landed),
      replace `openModal` with two functions:
      - `openConfirmModal({ title, message, confirmLabel, danger, onConfirm })`
        — string body only; one OK button + Cancel; renders the
        delete-confirm flow.
      - `openSaveAsModal(initialName)` — keeps its current shape
        (still takes `initialName`); inlines the name + folder form
        + Save button without going through a generic builder.
- [ ] `openDeleteConfirm(target)` calls `openConfirmModal`. Its
      body is the existing string (`Delete preset "${name}"? This
      cannot be undone.` etc.).
- [ ] `openSaveAsModal` is no longer a caller of a shared modal —
      it builds its own dialog directly. The construction code
      (header, name row with Edit button, folder dropdown, Save +
      Cancel buttons, backdrop, escape handling) lives in this
      function. Reuse the small shared helpers (e.g. an internal
      `createBackdrop()` if useful, but only if it reads simpler
      than inlining).
- [ ] One `modalEl` module-local still tracks "is a modal open"
      so the existing ESC cascade + the corpus-refresh-closes-modal
      behaviour keep working. Both new functions assign to
      `modalEl` on open and clear on close.
- [ ] `extendActions` / `setEnabled` are gone.
- [ ] The save-as flow gates the Save button via a direct
      `okBtn.disabled = !valid()` toggle from the `nameLabel`
      update callback. No extra indirection layer.
- [ ] Substring tests
      ([crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs))
      that mention `extendActions`, `openModal`, or `openSaveAsModal`
      update to reflect the new function names. The test
      `faceplate_save_as_modal_wired` (which already references
      `openSaveAsModal` and asserts on `.save-as-select` /
      `folderOptions` etc.) keeps working; the `extendActions`
      assertion gets dropped.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The `confirmLabel` parameter on the delete-confirm side stays
because "Delete" vs "OK" matters semantically.

`danger` on the OK button stays as a parameter on
`openConfirmModal` — applies the `danger` class to the OK button
so the deletion confirm reads red.

If `openConfirmModal`'s shape ends up identical to what `openModal`
used to be minus the function-body polymorphism, that's fine — the
goal is "one caller, one path" not "two new abstractions". Don't
try to invent the next shared primitive.
