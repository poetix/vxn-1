---
id: "0066"
title: Fold free-run and notch dims into DIM_RULES
priority: high
created: 2026-06-01
epic: E014
---

## Summary

Today the ViewEvent dispatcher special-cases three dim rules
inline (`ev.id === FREE_RUN_ID`, `ev.id === FILTER_MODE_ID`,
`applyDimRulesFor(ev.id, ev.plain)` for the rest) plus three
parallel data structures (`FREE_DIMMED_CELLS`, `SLOPE_DIMMED_CELLS`,
`DIM_RULES`). All four are the same shape: *when watched param's
predicate fires, toggle the `dimmed` class on these cells*. Unify
under one `DIM_RULES` table populated from a mix of HTML data
attributes (the current generic rules) and a hard-coded spec list
(the two bespoke ones).

## Acceptance criteria

- [ ] [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js):
      `DIM_RULES` becomes the single source of truth — entries
      `{ watchId, predicate, target }` where `target` is one element
      (the current generic shape).
- [ ] Add a second `DIM_RULE_SPECS` entry kind:
      ```js
      // Built-in dim specs that don't fit the HTML-attribute model
      // (selectors live across multiple cells; predicate is bespoke).
      // Each entry resolves to N targets * 1 watch.
      const BUILTIN_DIM_SPECS = [
        {
          kind: 'free-run',
          watch: 'lfo1_free_run',
          predicate: (plain) => plain >= 0.5,
          targets: ['lfo1_delay_time', 'lfo1_fade'],
        },
        {
          kind: 'filter-notch',
          watch: 'filter_mode',
          predicate: (plain, layer) =>
            Math.round(plain) === variantIdx('filter_mode', 'Notch', layer),
          targets: ['filter_slope'],
        },
      ];
      ```
      (`variantIdx` arrives in 0065 — sequence accordingly.)
- [ ] `resolveDimRules(layer)` (renamed if useful to
      `rebuildDimRules(layer)`) walks both `DIM_RULE_SPECS` and
      `BUILTIN_DIM_SPECS`, resolves each spec into one-or-more
      `{ watchId, predicate, target }` entries pushed into a single
      `DIM_RULES` list. Multi-target builtins fan out into N entries
      sharing the same `watchId` and `predicate`.
- [ ] `FREE_RUN_ID`, `FREE_DIMMED_CELLS`, `FILTER_MODE_ID`,
      `FILTER_NOTCH_INDEX`, `SLOPE_DIMMED_CELLS`, `locateFreeRunCells`,
      `locateSlopeDimCells` all deleted.
- [ ] Dispatcher in `init()`'s `dispatch` function loses the two
      `ev.id === FREE_RUN_ID` and `ev.id === FILTER_MODE_ID` branches.
      The single `applyDimRulesFor(ev.id, ev.plain)` call covers all
      four kinds.
- [ ] `rebindAllForLayer(layer)` no longer calls `locateFreeRunCells`
      / `locateSlopeDimCells` — just `rebuildDimRules(layer)` once.
- [ ] `refreshAllDimRules` works unchanged (it loops `DIM_RULES`
      reading from `LAST_PARAM`).
- [ ] Substring tests in
      [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs)
      that mention `locateSlopeDimCells`, `FILTER_MODE_ID`,
      `variants.indexOf('Notch')`, `ev.id === FILTER_MODE_ID` update
      to assert the equivalent post-refactor markers (e.g.
      `BUILTIN_DIM_SPECS`, `'filter-notch'`, `'free-run'`,
      `kind: 'free-run'`). Mechanical update.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The two builtin specs differ from HTML-attribute specs in that
their targets are *param names* (resolved at bind time to cells)
rather than DOM elements picked up by `document.querySelectorAll`.
The HTML-attribute path uses `el` directly; the builtin path
needs an extra step in resolve: `document.querySelector(\`[data-param="${name}"]\`)`
per target. Both produce `{ watchId, predicate, target: el }`
entries that look identical to the apply loop.

The `predicate` signature gains an optional `layer` parameter so
the notch lookup can resolve at apply time — *or* the lookup is
done once at resolve time and the predicate closes over the
captured `notchIdx`. Either works; the closed-over form is
slightly more efficient and matches how `unless-fm` already
closes over `fmIdx`. Pick that.

After this, the dispatch function's body shrinks measurably and
the four dim-rule kinds read as a single uniform mechanism.
