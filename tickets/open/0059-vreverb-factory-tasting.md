---
id: "0059"
title: Factory presets — reverb defaults + per-type tasting pass
priority: medium
created: 2026-06-01
epic: E012
---

## Summary

Audit the factory preset bank now that reverb exists. Defaults
land `reverb_on=0` (so existing presets are bit-exact through
the new bus); a handful of candidate presets gain a tasteful
per-type voicing so the new effect ships with audible presence,
not as silent overhead waiting to be discovered.

Depends on 0055–0058 (engine + UI must be live so the values can
be auditioned in the running plugin).

## Acceptance criteria

- [ ] All existing `crates/vxn-engine/presets/factory/**/*.toml`
      load cleanly after the four new globals are added; the
      preset loader treats absent reverb fields as defaults
      (`reverb_on=0`, type Plate, depth 0.5, mix 0.3).
- [ ] No automated-test regression — factory presets still
      hash-match their pre-reverb dry-bus outputs sample-exact
      when `reverb_on` resolves to 0.
- [ ] Hand-pick 3–6 factory presets where reverb meaningfully
      improves the patch, and edit their TOMLs in-place to set
      `reverb_on=1` with a tasteful type + depth + mix. Suggested
      starting set (final picks to be made by ear):
      - One pad → Hall, depth 0.7, mix 0.35
      - One keys/EP → Plate, depth 0.4, mix 0.25
      - One lead → Room, depth 0.3, mix 0.20
      - One ambient / texture → Large, depth 0.85, mix 0.45
- [ ] Re-run whatever script regenerates the embedded factory
      bank (per [[vxn1-preset-system]] — ADR 0005 / E007).
- [ ] Smoke-check each touched preset in the live plugin: load,
      hold a note, confirm the tail audibly matches the intent
      written in this ticket.

## Notes

Plate is the most "polite" voicing — fastest decay, brightest,
shortest. Default Type stays Plate to match.

The four picks above are illustrative. The real list emerges
from a 10-minute play-through of the bank. The criterion is
"presets that already sound dry / closed-in"; don't add reverb
to ones that already have intentional intimacy.

If a preset's pre-reverb mix already feels saturated, prefer a
lower `reverb_mix` over disabling the effect — the macro UI
discourages users from disabling reverb entirely, so banks
should normalise to "always-on, tastefully restrained".

No DSP / engine / UI changes in this ticket. If anything in
this audit reveals a macro-mapping value that's wrong (e.g.
Hall feels too dark, Large feels too long), open a follow-up
ticket against 0056's table — don't fold the fix into a preset
edit.
