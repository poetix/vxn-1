---
id: E011
title: Plugin management redesign (Phase C — preset browser, key-mode, text input)
status: open
created: 2026-05-30
---

## Goal

Close the remaining seam from E010. The synth panels are HTML; the
preset bar, key-mode panel and Upper/Lower edit toggle still render
via the legacy Vizia overlay. This epic ports them — with **a fresh
design** rather than a port — into the HTML faceplate, solves
host-keyboard-capture for text fields via a floating NSWindow popup,
and retires `vxn-ui-vizia`.

Decisions recorded in [ADR 0007 §7](../../adrs/0007-vxn1-mvc-architecture.md)
and (still relevant for the format) [ADR 0006](../../adrs/0006-vxn1-preset-browser-ergonomics.md).

## Background

Two things differ from "just port these too":

**The Vizia preset browser never reached a shippable state.** Open
tickets 0024–0032 describe ergonomic debt around the two-pane layout,
the rename/move/delete flow, drag-and-drop. Porting it to HTML
verbatim locks in those weaknesses; a redesign is cheaper here because
the controller already owns the corpus IO (E009 / 0038) — the view
is a thin projection.

**Text input under DAW hosting is a known dead end inside the plugin
view.** DAWs install NSEvent monitors on key events for transport;
the host swallows Space, Enter etc. before any child view sees them.
The workaround is a **floating NSWindow** with an NSTextField — its
own key window, outside the host's monitor scope. Standard trick
across Spitfire / Output / Arturia for preset rename.

Once both are HTML-side, the Vizia editor has no purpose and the
crate retires.

## In scope

- HTML preset bar (current preset name display, prev/next walkers,
  open browser button, save-as form).
- HTML preset browser panel (two-pane folders / presets, search box,
  rename / move / delete flows, drag-and-drop). Redesign latitude —
  not a Vizia port.
- HTML keys panel (mode selector Whole/Dual/Split, Upper/Lower edit
  target toggle, split-point slider).
- Floating NSWindow popup for text input (rename, save-as, new
  folder). Used by anywhere the user types — sidesteps host kbd
  capture.
- Drop `vxn-ui-vizia` crate; flip vxn-clap default feature to
  `webview`.
- Memory cleanup: archive the four Vizia-specific bug memories
  (`vxn1-vizia-no-click-slop`, `vxn1-vizia-automation-relayout-input-stomp`,
  `vxn1-vizia-absolute-stretch-overlay`, `vxn1-vizia-layout-probe-tool`)
  with a note that they apply to the retired Vizia editor only.

## Out of scope

- Cross-platform parity for the text popup. macOS NSWindow is the
  prototype. Windows + Linux equivalents (CreateDialog,
  GtkDialog) sized to per-platform tickets later.
- VXN-2 work.
- Any audio-engine change.

## Phasing

- **0048** Floating NSWindow text-input popup (objc, macOS-only at
  first). Used by everything in this epic that needs typing.
- **0049** HTML preset bar (current name, prev/next, browser toggle,
  save-as — uses 0048 popup for the name field).
- **0050** HTML preset browser panel — folders / presets two-pane,
  search box.
- **0051** Rename / move / delete flows in browser (uses 0048
  popup for rename + new folder name input).
- **0052** Drag-and-drop preset → folder (HTML5 drag, posts
  `UiEvent::MovePreset` to controller).
- **0053** HTML keys panel — mode selector + edit toggle + split.
- **0054** Retire vxn-ui-vizia; flip default feature.

## Acceptance

- `./deploy.sh` (no flag) produces a CLAP whose editor is entirely
  HTML — synth panels, preset bar, keys, browser. No Vizia overlay.
- Preset rename, save-as, new-folder name input all work in every
  major DAW (Bitwig, Live, Reaper, Logic) — including names with
  spaces and special characters.
- Preset browser supports search, sort, rename, delete, move, drag-
  drop with controller-mediated IO.
- `cargo test --workspace` passes.
- `crates/vxn-ui-vizia/` deleted; workspace member list updated;
  cargo features cleaned up.
- Memory files for Vizia-specific bugs archived (kept for history,
  marked obsolete).
