---
id: "0067"
title: Group dispatch state into one model object
priority: medium
created: 2026-06-01
epic: E014
---

## Summary

`dispatch.js` has 12+ module-level mutables — `CONTROLS`, `LAST_PARAM`,
`SYNC_OF_RATE`, `RATE_OF_SYNC`, `CURRENT_LAYER`, `DIM_RULE_SPECS`,
`DIM_RULES`, `LAYERED_CELLS`, `STATIC_CELLS` (plus, pre-0066, the
free-run / filter-notch state). They're the dispatcher's *model*.
Group them under one `model` object so the names read as fields of
the dispatch state rather than free-floating globals, and a future
read of the module shows what dispatch owns at a glance.

Pure renaming + nesting; no behavioural change.

## Acceptance criteria

- [ ] [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js):
      define one `model` near the top of the file:
      ```js
      // Per-tick mutable state the dispatcher owns. Grouped here so the
      // module reads as "init builds the model; dispatch reads + mutates
      // it" rather than 12 free-floating globals.
      const model = {
        // ParamChanged routing: id → [updater closures].
        controls: new Map(),
        // Last (plain, norm, display) seen per id, indexed by CLAP id.
        // Sync flip / dim refresh / layer rebind reseed from here.
        lastParam: new Map(),
        // sync_partner pairings: rateId ↔ syncId for LFO1 / LFO2 / Delay.
        // Resolved per layer in rebindAllForLayer.
        syncOfRate: new Map(),
        rateOfSync: new Map(),
        // Active edit layer ('upper' | 'lower'). EditLayerChanged mutates.
        currentLayer: 'upper',
        // Dim-rule specs collected from HTML attributes + builtins (0066).
        dimRuleSpecs: [],
        // Resolved rules for the current layer: { watchId, predicate, target }.
        dimRules: [],
        // Per-cell binding info captured at init; rebuilt against new
        // layer ids on EditLayerChanged.
        layeredCells: [],
        staticCells: [],
      };
      ```
      (After 0072 lands, `layeredCells` + `staticCells` collapse to
      one `cells` field — sequence accordingly.)
- [ ] Replace every reference to the former globals with the
      matching `model.*` field. `addCtl` becomes a helper that
      mutates `model.controls`. `CURRENT_LAYER` → `model.currentLayer`.
      Etc.
- [ ] `init()` builds the model; `dispatch()` reads + mutates it.
      No global `let CURRENT_LAYER = …` or `const LAST_PARAM = new Map()`
      remaining.
- [ ] Substring tests in
      [crates/vxn-ui-web/src/lib.rs](../../crates/vxn-ui-web/src/lib.rs)
      that check for symbol names (e.g. `rebindAllForLayer`,
      `paramIdByNameAtLayer`, `applyDimRulesFor`, `collectDimRuleSpecs`)
      keep working — those are functions, not state, and stay
      module-level. Any test asserting on a state-symbol name
      (e.g. the *names* `LAST_PARAM`, `CONTROLS`, `CURRENT_LAYER`)
      updates to the new `model.…` form.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

The model isn't reactive — it's a plain object the dispatcher
reads and writes. Adding observers is out of scope.

Helpers like `paramIdByName`, `paramIdByNameAtLayer`, `variantIdx`
(from 0065), `isLayeredEl` stay module-level functions — they're
stateless, they don't belong in the model.

The split between functions (module-level) and state (`model`)
makes it visible what's per-tick mutable vs invariant. A future
reader skims the function list to learn the operations, then
reads `model` to learn the data.

`addCtl` becomes either `model.addCtl(id, ctl)` (method on the
model) or a free helper that takes `model.controls` — either is
fine; pick the form that reads simpler at the call sites in
`bindCell`.
