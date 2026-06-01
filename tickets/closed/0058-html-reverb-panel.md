---
id: "0058"
title: HTML faceplate — Reverb panel in row 4, tuned flex shares
priority: high
created: 2026-06-01
epic: E012
---

## Summary

Add a Reverb panel between Delay and Master on row 4 of the HTML
faceplate. Three body cells (Type buttongroup, Depth fader, Mix
fader) plus a header `On` switch matching the Chorus / Delay
idiom. Tune the row's `flex-grow` shares so the six panels fit
without a cramped Type buttongroup.

Depends on 0056 (the four globals exist) and 0057 (the engine
consumes them — without 0057 the controls render but are inert).

## Acceptance criteria

- [ ] New panel inserted in
      [crates/vxn-ui-web/assets/faceplate.html](crates/vxn-ui-web/assets/faceplate.html#L1341),
      between the Delay panel and the Master panel:
      ```html
      <div class="panel" data-name="Reverb" data-header-toggle>
        <div class="panel-header">
          <div class="panel-header-toggle-slot"
               data-control="header-switch"
               data-param="reverb_on"></div>
          <div class="panel-header-title">REVERB</div>
        </div>
        <div class="panel-body">
          <div class="ctl" data-control="buttongroup"
               data-param="reverb_type" data-label="Type"></div>
          <div class="ctl" data-control="fader"
               data-param="reverb_depth" data-label="Depth"></div>
          <div class="ctl" data-control="fader"
               data-param="reverb_mix" data-label="Mix"></div>
        </div>
      </div>
      ```
- [ ] Row-4 flex shares written explicitly (replace the implicit
      `flex: 1 1 0`):
      ```css
      .row-4 .panel[data-name="Keys"]   { flex-grow: 1.00; }
      .row-4 .panel[data-name="Voice"]  { flex-grow: 0.85; }
      .row-4 .panel[data-name="Chorus"] { flex-grow: 1.00; }
      .row-4 .panel[data-name="Delay"]  { flex-grow: 1.10; }
      .row-4 .panel[data-name="Reverb"] { flex-grow: 0.95; }
      .row-4 .panel[data-name="Master"] { flex-grow: 0.90; }
      ```
      Verified by running and nudged after — values are a starting
      point.
- [ ] Test updates in
      [crates/vxn-ui-web/src/lib.rs](crates/vxn-ui-web/src/lib.rs):
      - `faceplate_reserves_chorus_delay_header_toggle`
        ([line 1020](crates/vxn-ui-web/src/lib.rs#L1020)) — extend
        the `for name in [...]` array to include `"Reverb"`; rename
        the test to `faceplate_reserves_fx_header_toggles`; update
        the assert message.
      - Header-switch count assertion
        ([line 1660](crates/vxn-ui-web/src/lib.rs#L1660)) — bump
        `expected 2 header-switch cells (Chorus, Delay)` to
        `expected 3 header-switch cells (Chorus, Delay, Reverb)`
        with the matching numeric bump.
      - Row-4 cell list
        ([line 1611](crates/vxn-ui-web/src/lib.rs#L1611)) — add
        Reverb's 1 header-switch + 1 buttongroup + 2 faders to
        the per-row tally; update the row-4 grand total.
      - FX cell list
        ([line 1543](crates/vxn-ui-web/src/lib.rs#L1543)) — add
        the four reverb entries:
        ```rust
        ("header-switch", "reverb_on",    ""),
        ("buttongroup",   "reverb_type",  "Type"),
        ("fader",         "reverb_depth", "Depth"),
        ("fader",         "reverb_mix",   "Mix"),
        ```
- [ ] `cargo test -p vxn-ui-web` passes.
- [ ] Manual verification (per global feedback guidance — ask
      before screen capture): run the CLAP host, confirm the
      panel renders, all four controls respond, header switch
      bypasses cleanly, no clipping or layout overflow on the
      default window size.

## Notes

The Type buttongroup carries four labels (Plate / Room / Hall /
Large) — same widget as `oversample` and `assign_mode`. No new
control primitive needed.

Flex shares are a taste call; the values above shrink Voice +
Master slightly (both have light internal density) and give
Delay the most room because of its strip row underneath. Likely
needs one or two passes after seeing it on the real faceplate;
don't over-tune them in the patch — easy to nudge.

The header-switch idiom is already established for Chorus + Delay;
follow-up cleanup of test names (`chorus_delay_*` → `fx_*`) is
worth doing while you're already in the file, but minor.
