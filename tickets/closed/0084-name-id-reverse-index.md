---
id: "0084"
title: Build name→id reverse index once in `init()`
priority: low
created: 2026-06-01
epic: E016
---

## Summary

The 0075 audit's finding N5:
`paramIdByName` ([dispatch.js:6–11](../../crates/vxn-ui-web/assets/dispatch.js#L6-L11))
walks every entry in `window.vxn.params` linearly. `bindCell` and
`rebindAllForLayer` invoke it per cell per layer flip; `variantIdx`
invokes it per dim-rule rebuild. Today's param count (~150) and
cell count (~50) keep the cost invisible, but a cached reverse
index makes the code read as "look up", not "scan", and removes a
forward concern when vxn-2 grows the param table.

Build the index once in `init()`; clear and rebuild if `window.vxn.params`
is ever reassigned (it isn't today, but the contract should hold).

## Acceptance criteria

- [ ] [dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)
      add a module-level `_paramIdByName = null` cache and a
      `buildParamIndex()` function that walks `window.vxn.params`
      once and returns a `Map<string, number>` (name → lowest id;
      for a per-patch param this is the Upper-layer id).
- [ ] `paramIdByName(name)` becomes:
      ```js
      function paramIdByName(name) {
        if (_paramIdByName == null) _paramIdByName = buildParamIndex();
        const id = _paramIdByName.get(name);
        return id == null ? null : id;
      }
      ```
- [ ] `init()` calls `_paramIdByName = buildParamIndex()` before
      `rebindAllForLayer` so the first rebind already hits the
      cache.
- [ ] `paramIdByNameAtLayer` and `variantIdx` are unchanged at
      call-site (they call `paramIdByName` internally).
- [ ] [crates/vxn-ui-web/assets/__tests__/param-id-by-name.test.js](../../crates/vxn-ui-web/assets/__tests__/param-id-by-name.test.js)
      covers:
      - Builds against the 0080 fixture params table; asserts
        every name maps to the expected id.
      - Unknown name returns `null`.
      - First call builds the index; second call hits the cache
        (spy on `buildParamIndex` to assert exactly one call
        across many lookups).
      - `paramIdByNameAtLayer` translates Upper → Lower
        (`+patchCount`) for per-patch ids; passes globals
        through unchanged.
- [ ] Manual smoke (ask first): faceplate boots, layer flips
      still rebind correctly, dim rules still resolve.
- [ ] `npm test` and `cargo test -p vxn-ui-web` pass.

## Notes

E017/0087 (wrap params descriptor) will subsume this — the cached
index becomes a method on the `params` object. Sequencing: if
0087 lands first, 0084 is a no-op (close as "subsumed by 0087").
If 0084 lands first, 0087 promotes the cache to the model object
and removes the module-level `_paramIdByName`.

Don't add a "reset on params change" hook — `window.vxn.params`
is set once at editor open and never reassigned. If that ever
changes, the cache is a one-line invalidation away.
