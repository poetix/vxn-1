---
id: "0050"
title: HTML preset browser panel — folders / presets two-pane + search
priority: high
created: 2026-05-30
epic: E011
---

## Summary

Open the 0049 Browse button into a floating preset browser panel:
left pane lists folders (Factory categories + User folders), right
pane lists presets in the selected folder, search box filters by
name (case-insensitive substring match). Folder selection +
search are pure view state; everything else is controller-mediated.

This is a **redesign**, not a port. The Vizia browser's idioms
(`browser-pane`, `browser-section`, etc.) inform but don't bind.

## Acceptance criteria

- [ ] Panel opens / closes from 0049's Browse button.
- [ ] Left pane: Factory header + sorted categories, User header +
      "Uncategorised" first then sorted folders. Click selects.
- [ ] Right pane: name-sorted list of presets in the selected
      folder. Click loads (posts `UiEvent::LoadPreset { source }`).
- [ ] Search box: substring match on `meta.name`, lowercased,
      across the selected folder. Clear button resets.
- [ ] Panel scrolls if content overflows; ESC closes; clicking
      outside closes.
- [ ] Folder + preset corpus sourced from controller (`ViewEvent::
      PresetCorpusChanged` rebuilds the rendered lists).
- [ ] Currently-loaded preset highlighted in the right pane when
      its folder is selected.

## Notes

This ticket is "browse only". Rename / delete / move land in 0051;
drag-drop in 0052. The "redesign latitude" mostly cashes in there.
Browse-only is more constrained — the data shape is fixed by ADR 0006.

CSS-wise, the floating panel sits inside the WebView document (not
a separate native window). It can absolutely-position over the rows
without z-index drama; standard HTML.
