---
id: E016
title: Faceplate JS post-E014 boundary cleanup
status: open
created: 2026-06-01
---

## Goal

Land the fresh findings raised in the 0075 close-out audit (E014):
five real cleanup items plus two one-line trims. Each landed change
ships a Vitest assertion covering the new surface — the test net
from [E015](E015-js-test-framework.md) is the prerequisite, and
this epic is its first sustained user.

Behavioural change is **out of scope** — every panel, control, and
event flow must work bit-identically afterwards. The Vitest suite
plus the substring suite are the regression nets.

## Background

The 0075 close-out audit surfaced nine fresh findings (N1–N9 in the
ticket comment) that emerged from the E014 cleanup or were obscured
by the original mess. The four boundary findings (N2 wave-drag
re-implementation, N3 detune-legato tg-row inline, N4 thumb-math
duplication, N5 `paramIdByName` linearity) are sized for follow-up
tickets — small in code, high in clarity. The two inline trims (N8
dead `_browserOpen`, N9 redundant ternary) are sized for one ticket.

The remaining audit findings (N1 primitive ↔ global coupling, N6
cells-list dual purpose, N7 modal mount target) are forward-looking
notes for [E017](E017-js-reusable-primitives.md) and don't appear
here.

## In scope

- N8 dead `_browserOpen` write — delete.
- N9 `target.kind === 'preset' ? target.name : target.name` —
  collapse.
- N2 generalise drag scaffolding so `makeWave` reuses the same
  shape `wireFaderDrag` already provides.
- N4 `paintFader(fader, thumb, norm)` helper — collapse the two
  copies of `halfThumb + (1 - n) * travel`.
- N5 build name → id index once in `init()` — `paramIdByName`
  off the rebind hot path.
- N3 `tgRow` accepts a target / returns innerHTML so
  `makeDetuneLegato` can use it for the LEGATO toggle row.

## Out of scope

- Bridge injection / primitive parameterisation — that's E017.
- Reorganising into packages — E017.
- Behaviour or wire-shape changes.

## Phasing

E015 must land first (0076–0078 at minimum; 0079/0080 in parallel
with E016 is fine — each E016 ticket exercises the relevant
helpers).

1. **0081** Inline trims (N8 + N9). One small ticket; no test
   surface added.
2. **0082** Generalise drag scaffolding (N2). Adds
   `wireDrag(el, { pointerToValue }, callbacks)` that subsumes
   `wireFaderDrag` and removes the parallel implementation in
   `makeWave`. Tests added against the generalised helper.
3. **0083** `paintFader` helper (N4). Collapses `setThumb` and
   `setThumbFromPlain`. Tested against jsdom-mounted faders.
4. **0084** Name → id reverse index in `init()` (N5). Tested with
   a fixture params table.
5. **0085** `tgRow` accepts target / returns container (N3).
   `makeDetuneLegato` uses it. Tested for both standalone and
   composite usage.

0082 / 0083 can land in either order. 0081 / 0084 / 0085 are
independent.

## Tickets

- [ ] [0081 — Inline trims: dead `_browserOpen`, redundant ternary](../../tickets/open/0081-inline-trims.md)
- [ ] [0082 — Generalised `wireDrag` covers fader and wave](../../tickets/open/0082-generalised-wire-drag.md)
- [ ] [0083 — `paintFader` helper collapses thumb math](../../tickets/open/0083-paint-fader-helper.md)
- [ ] [0084 — Name→id reverse index in init()](../../tickets/open/0084-name-id-reverse-index.md)
- [ ] [0085 — `tgRow` accepts target; detune-legato uses it](../../tickets/open/0085-tg-row-target.md)

## Acceptance

- All five tickets closed.
- `npm test` under `crates/vxn-ui-web/assets/` passes; the suite
  has grown to cover each new helper.
- `cargo test -p vxn-ui-web` passes.
- No remaining bare `Math.max(0, Math.min(1, 1 -` outside the
  generalised drag helper.
- No remaining `halfThumb + (1 - n) * travel` outside
  `paintFader`.
- `paramIdByName`'s linear scan happens once, in `init()` — every
  per-cell / per-rebind lookup hits the cached index.
- Manual smoke confirms zero regression (ask first per
  `ask-before-screen-capture`).
