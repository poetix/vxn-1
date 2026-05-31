---
id: "0051"
title: Browser rename / move / delete + new folder flows
priority: high
created: 2026-05-30
epic: E011
---

## Summary

Add the user-side mutation flows to the 0050 browser: right-click
context menu on a row offers Rename, Delete, Move to ▸; a "New
folder" button on the user folder header opens 0048's text popup.
Delete uses the existing two-click confirm pattern from the Vizia
version (single click queues, second confirms within 3 s).

## Acceptance criteria

- [ ] Right-click on a user preset opens menu with Rename, Delete,
      Move to ▸ (submenu of other folders).
- [ ] Right-click on a user folder opens menu with Rename, Delete.
      Factory rows have no menu (read-only).
- [ ] Rename opens 0048 popup with the existing name pre-filled;
      commit posts `UiEvent::RenamePreset` or `RenameFolder`.
- [ ] Move ▸ submenu lists every user folder except the current one
      ("Uncategorised" first, then alpha); click posts
      `UiEvent::MovePreset { source, target }`.
- [ ] Delete: first click sets the row to a "Click to confirm" state
      with a 3 s timeout; second click within that window posts
      `UiEvent::DeletePreset` or `DeleteFolder`.
- [ ] New Folder button on the user header opens 0048 popup; commit
      posts `UiEvent::NewFolder { name }`.
- [ ] After every mutation the controller emits
      `ViewEvent::PresetCorpusChanged` and the panel re-renders.
- [ ] Status line surfaces controller-emitted error messages
      (e.g. "rename failed: name in use").

## Notes

The delete-confirm timeout was a real ergonomic improvement in the
Vizia version (`vxn1-preset-system` notes); the auto-clear pattern
in `DeleteSweeper` is what makes it feel right. Port the timing,
not the implementation.

Drag-drop is 0052; this ticket is menu-driven only.
