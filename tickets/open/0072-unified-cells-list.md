---
id: "0072"
title: One unified cells list with layered:bool
priority: low
created: 2026-06-01
epic: E014
---

## Summary

`dispatch.js` keeps two parallel arrays — `LAYERED_CELLS` and
`STATIC_CELLS` — of the same record shape. The discriminator is
"does this cell's element have a `[data-layered]` ancestor?".
`rebindAllForLayer` walks them sequentially with identical logic.

Replace with one `cells: []` list whose entries carry `layered: bool`.
The rebind loop still has to know which to skip on a layer flip
(static cells don't rebind), but it's one `if` not two loops.

Trivial declutter; depends loosely on 0067 (group dispatch state)
since the unified list lives inside `model`.

## Acceptance criteria

- [ ] [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js):
      replace `model.layeredCells` + `model.staticCells` with
      `model.cells = []`. Each entry: `{ el, kind, name, layered, extras? }`.
- [ ] `init()`'s `document.querySelectorAll('[data-control]')` loop:
      ```js
      const entry = { el, kind, name, layered: isLayeredEl(el) };
      if (kind === 'detune-legato') {
        entry.extras = {
          legatoName: el.dataset.legatoParam,
          modeName: el.dataset.modeParam,
        };
      }
      model.cells.push(entry);
      ```
- [ ] `rebindAllForLayer(layer)`: one loop over `model.cells`. The
      reset block (`el.innerHTML = ''`, `removeAttribute('style')`,
      `classList.remove(…)`) runs only for `layered` cells (static
      cells don't need it on a layer flip since they're never rebound
      anyway). The `bindCell(entry, layer)` call runs for both —
      *but* only on first init for static cells.
- [ ] Cleanest framing: on the *first* call (init time), bind both
      static and layered. On subsequent calls (layer flips), rebind
      only layered cells. Track via a `static_bound: bool` on `model`
      or by passing a flag. The current code happens to bind static
      cells on every `rebindAllForLayer` call too; that's wasted
      work but harmless. Pick whichever reads simpler — if "always
      bind both" reads cleaner, keep it (the wasted work is tiny);
      if "bind static once" reads cleaner, add the flag.
- [ ] If 0067 hasn't landed yet: this ticket also adds the `model.*`
      naming for `cells` (i.e. 0067's grouping is partial-applied for
      this one piece of state). Mark the dependency in 0067's notes
      if it lands second.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The two-arrays shape is a transcript of how 0045 added per-patch
support after the initial layer-agnostic version landed — at the
time it was the smallest possible change. Now that the layered/static
mix is stable, one list with a flag is clearer.

`isLayeredEl(el)` stays as the predicate that resolves the flag at
collection time.

No behaviour change. The rebind already handles static + layered
in lockstep; this just makes the lockstep visible.
