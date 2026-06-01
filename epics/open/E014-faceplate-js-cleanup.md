---
id: E014
title: Faceplate JS cleanup — duplication, domain entities, declutter
status: open
created: 2026-06-01
---

## Goal

Reduce duplication and clutter in the three faceplate JS modules
([bridge.js](../../crates/vxn-ui-web/assets/bridge.js),
[panels.js](../../crates/vxn-ui-web/assets/panels.js),
[dispatch.js](../../crates/vxn-ui-web/assets/dispatch.js)), surface
the bridge protocol and a handful of domain pegs (variant names,
twin-detune ceiling, dim-rule predicates) as named entities, and
collapse the parts of the dispatch / control-primitive code that
read as transcripts of the same five-line dance.

Behavioural change is **out of scope** — every panel, control, and
event flow must work bit-identically afterwards. The substring
suite in `vxn-ui-web::tests::faceplate_*` is the regression net;
when an assertion is no longer meaningful after a rename it gets
updated as part of the same ticket.

## Background

The faceplate JS landed incrementally across 0040–0058 (one panel
or feature per ticket); recently 0054 retired Vizia and the JS is
now the only editor. The cumulative shape was a 3500-line single
inline `<script>`; 2026-06-01's pre-epic refactor split it into
the three modules above. With the split visible, a review of the
JS surfaced 16 findings across three themes:

- **Duplication** — gesture-bracketed `set_param` writes (13 sites
  with the same `begin → set → end` triplet); fader drag scaffolding
  duplicated between `makeFader` and `makeDetuneLegato`; the
  `hovered/dragging + valuePop.show/update/hide` lifecycle written
  out three times; small clamp / pointer-norm one-liners repeated.
- **Domain entities not named** — opcode strings (`'set_param'`,
  `'begin_gesture'`, …) at every call site; variant-by-name
  lookups (`variants.indexOf('Notch')`, `'FM'`, `'Twin'`, …)
  scattered; the four dim-rule kinds split between three
  hard-coded dispatch branches and one data-driven specs table;
  the dispatch-side state (`LAST_PARAM` / `CONTROLS` / sync maps
  / dim ids / cell lists) is twelve bare module-level mutables.
- **Decluttering** — `browserPanel` is ~770 lines doing six
  concerns; `openModal({body: fn|str, extendActions})` exists
  only because Save-As needs an enable hook; magic numbers
  (status-pill 3s, knob indicator 120ms) and `TWIN_TOP_CT` sit
  inside functions instead of beside the other domain constants
  at the top of the file.

None of these are bugs — the editor works. The change is
readability and the friction of future work in these files.

## In scope

All 16 findings from the review (2026-06-01), grouped into 12
implementation tickets (0063–0074) plus a final re-review (0075).

## Out of scope

- HTML restructuring or panel layout changes.
- CSS changes.
- Adding new controls or panels.
- Any change to the JS→Rust IPC wire shape (opcode strings stay
  identical; the senders are typed wrappers that emit the same
  JSON). Same for ViewEvent `kind` strings.
- Splitting `panels.js` further (browser → its own file is the
  one exception, justified by size; the rest stays in `panels.js`).
- Type checking / TypeScript / a build step. JS stays as JS that
  the wry WebView evaluates verbatim.

## Phasing

Tickets are roughly ordered by impact (high-ROI first) and
dependency. The split:

1. **0063** Typed sender API (`vxn.send.setParam(id, plain)`,
   `vxn.send.beginGesture(id)`, …). Foundation for 0064; also
   collapses ad-hoc opcode-string call sites everywhere else.
2. **0064** `discrete(id, plain)` helper for gesture-bracketed
   one-shots. Collapses the 13 `begin → set → end` triplets.
   Depends on 0063 (uses its typed senders internally).
3. **0065** `variantIdx(paramName, variantName, layer)` helper.
   Surfaces the half-dozen `variants.indexOf('…')` lookups.
4. **0066** Fold `lfo1_free_run` dim and `filter_mode === Notch`
   dim into the generic `DIM_RULES` machinery — three special-
   cased dispatch branches become two data-driven specs.
5. **0067** Group dispatch-side module-level state into a single
   `model` object. Pure rename / scoping; no behaviour change.
6. **0068** Extract `wireFaderDrag` — the pointer-capture / drag /
   release scaffolding shared by `makeFader` and `makeDetuneLegato`.
7. **0069** Extract `attachValuePop` for the hover / drag value-popup
   lifecycle shared by `makeFader`, `makeWave`, `makeDetuneLegato`.
8. **0070** Three small helpers — `clampVariant`, `pointerNorm` —
   plus the `tgRow()` miss in `keysPanel` (twice-inlined HTML).
9. **0071** Hoist magic numbers — status-pill flash duration,
   knob-indicator transition — and `TWIN_TOP_CT` to a constants
   block beside `KEYS_DEFAULT_SPLIT`.
10. **0072** One unified `cells` list with `layered: bool` instead
    of two parallel arrays (`LAYERED_CELLS`, `STATIC_CELLS`).
11. **0073** Move `browserPanel` to `browser.js`. Add `__BROWSER_JS__`
    placeholder + splice; ~770 lines out of `panels.js`.
12. **0074** Unbundle Save-As modal from the delete-confirm modal —
    the shared `openModal({extendActions})` only exists for the
    Save-button enable hook; two distinct functions is clearer.
13. **0075** Final re-review — re-walk the three modules against
    the 2026-06-01 findings, confirm each is addressed and no new
    duplication / leakage / domain-entity drift was introduced.

The order matters loosely: 0063 → 0064 must be sequenced (0064
uses the senders from 0063); 0068 / 0069 can land in either order;
the rest are independent and can be tackled in any sequence the
author prefers.

## Tickets

- [ ] [0063 — Typed sender API for IPC opcodes](../../tickets/open/0063-typed-senders.md)
- [ ] [0064 — `discrete(id, plain)` helper collapses gesture brackets](../../tickets/open/0064-discrete-helper.md)
- [ ] [0065 — `variantIdx` helper for variant-by-name lookups](../../tickets/open/0065-variant-idx-helper.md)
- [ ] [0066 — Fold free-run and notch dims into DIM_RULES](../../tickets/open/0066-unify-dim-rules.md)
- [ ] [0067 — Group dispatch state into one `model` object](../../tickets/open/0067-group-dispatch-state.md)
- [ ] [0068 — Extract `wireFaderDrag`](../../tickets/open/0068-wire-fader-drag.md)
- [ ] [0069 — Extract `attachValuePop`](../../tickets/open/0069-attach-value-pop.md)
- [ ] [0070 — Small helpers and `tgRow` miss in keysPanel](../../tickets/open/0070-small-helpers.md)
- [ ] [0071 — Hoist magic numbers and `TWIN_TOP_CT`](../../tickets/open/0071-hoist-constants.md)
- [ ] [0072 — One unified `cells` list with `layered: bool`](../../tickets/open/0072-unified-cells-list.md)
- [ ] [0073 — Move `browserPanel` to `browser.js`](../../tickets/open/0073-split-browser-js.md)
- [ ] [0074 — Unbundle Save-As modal from delete-confirm](../../tickets/open/0074-unbundle-modals.md)
- [ ] [0075 — Re-review faceplate JS against the original findings](../../tickets/open/0075-faceplate-js-re-review.md)

## Acceptance

- All twelve refactor tickets (0063–0074) closed.
- `cargo test -p vxn-ui-web` passes with the substring suite
  updated (renames, not removals) where helpers change the
  surface text.
- 0075's re-review report identifies no remaining instance of
  any 2026-06-01 finding and surfaces no new instance of the
  same patterns introduced as a side effect of the cleanup.
- Manual smoke (per the `ask-before-screen-capture` rule, ask
  first): load the plugin in a host, walk every panel, confirm
  every control still responds to clicks, drags, automation
  echoes, preset loads, layer flips, key-mode flips. No regressions.
