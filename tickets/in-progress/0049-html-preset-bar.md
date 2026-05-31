---
id: "0049"
title: HTML preset bar — current name, prev/next, browser toggle, save-as
priority: high
created: 2026-05-30
epic: E011
---

## Summary

Replace the Vizia preset bar overlay with an HTML preset bar in the
slot reserved by 0040. Carries the current preset name display, the
prev/next walkers over the combined Factory+User list, a "Browse"
button that toggles 0050's browser panel, and a "Save As" action
that opens the 0048 popup for the preset name.

## Acceptance criteria

- [ ] Renders in the row reserved between banner and Row 1
      (~30 px tall).
- [ ] Current preset name binds to `ViewEvent::PresetLoaded { meta, .. }`.
- [ ] Prev/Next walk the combined ordered list (factory then user,
      both alpha-sorted) — controller publishes the list, editor
      buttons post `UiEvent::StepPreset { delta }`.
- [ ] Browse button toggles a `browserOpen: bool` view-side state;
      0050 ticket binds the browser panel to it.
- [ ] Save As opens the floating popup (0048), commits to
      `UiEvent::SavePreset { name, folder: <current folder> }`.
- [ ] No Vizia preset bar code path runs in the webview build.

## Notes

The folder for Save As is whichever folder the browser panel
currently has selected, defaulting to user root. For 0049 (browser
not yet built), default to user root unconditionally; 0051 ties the
two together.

Status messages (Loaded "X", Save failed: …) render as a small chip
to the right of the name, fed by `ViewEvent::Status`. 3-second auto-
dismiss.
