---
id: "0065"
title: variantIdx helper for variant-by-name lookups
priority: medium
created: 2026-06-01
epic: E014
---

## Summary

Add `variantIdx(paramName, variantName, layer)` to
[dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js) and route
the six existing scattered lookups through it. Surfaces the
domain pegs (Notch, FM, Twin, Unison, Solo) as named entities of
the param model rather than `variants.indexOf('…')` calls buried
inside init / dim resolution / detune-legato.

## Acceptance criteria

- [ ] New helper in
      [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
      beside `paramIdByNameAtLayer`:
      ```js
      // Look up a variant's plain index on an enum param at the current
      // layer. Returns -1 if either the param or the variant name is
      // unknown — callers treat that as "rule does not apply".
      function variantIdx(paramName, variantName, layer) {
        const id = paramIdByNameAtLayer(paramName, layer);
        if (id == null) return -1;
        const variants = window.vxn.params[id].variants || [];
        return variants.indexOf(variantName);
      }
      ```
- [ ] Replace existing call sites:
      - [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
        `locateSlopeDimCells`: `const variants = …; FILTER_NOTCH_INDEX = variants.indexOf('Notch');`
        becomes `FILTER_NOTCH_INDEX = variantIdx('filter_mode', 'Notch', layer);`.
      - [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
        `resolveDimRules` unless-fm branch: replace the local
        `variants.indexOf('FM')` with a `variantIdx(spec.watchName, 'FM', layer)`
        captured into the predicate's closure.
      - [panels.js](../../crates/vxn-ui-web/assets/panels.js)
        `makeDetuneLegato`: `TWIN_IDX = modeVariants.indexOf('Twin')` →
        callers of the composite stay as-is (they get the descriptor
        already), but rebuild via `variantIdx(modeName, 'Twin', layer)`
        called once at bind time (passed through the existing
        `descs.mode.variants` path or, cleaner, the `bindCell`
        `detune-legato` branch resolves them via `variantIdx`).
      - Same for `Unison` and `Solo` in `MONO_IDXS`.
- [ ] If the cleanest path is to keep `makeDetuneLegato`'s lookups
      local (the composite is sui generis and stays in `panels.js`)
      but route them through a `panels.js`-local `lookupVariant`
      wrapper that calls into `variantIdx`, that's acceptable too —
      the goal is "one helper does the lookup" not "every caller
      crosses the file boundary." Whichever reads simpler.
- [ ] `cargo test -p vxn-ui-web` passes.

## Notes

There's an asymmetry: `FILTER_NOTCH_INDEX` is recomputed inside
`locateSlopeDimCells(layer)` because the layer flip changes
`FILTER_MODE_ID` (per-patch param). `variantIdx` takes `layer` so
the same call works inside any per-layer rebuild.

For globals (where layer doesn't matter), passing `'upper'` or
`CURRENT_LAYER` makes no difference since `paramIdByNameAtLayer`
no-ops for globals.

After this, grep for `variants.indexOf(` should hit `variantIdx`'s
definition and maybe one or two `makeDetuneLegato`-local helpers.
Bare `variants.indexOf('…')` literals in dispatch / dim-rule code
should be gone.
