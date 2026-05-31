---
id: "0054"
title: Retire vxn-ui-vizia; flip vxn-clap default to webview
priority: medium
created: 2026-05-30
epic: E011
---

## Summary

Delete the `vxn-ui-vizia` crate. Drop the `vizia` cargo feature on
`vxn-clap`. Flip the default editor backend to webview. Update
deploy.sh + xtask: the `--webview` flag becomes redundant (default
now) and is removed; a new `--vizia` flag is *not* added — there
is no Vizia path anymore.

## Acceptance criteria

- [ ] `crates/vxn-ui-vizia/` deleted; workspace member list, lock
      file, dependency graph updated.
- [ ] `vxn-clap` Cargo.toml has no `[features]` for the editor
      backend; `vxn-ui-web` is a plain dependency.
- [ ] `vxn-clap/src/lib.rs` + `gui.rs` use `vxn_ui_web` directly
      (no `vxn_editor` alias indirection).
- [ ] `xtask` drops `--webview` from its arg list.
- [ ] `deploy.sh` drops `--webview`; reverts to the one-flag
      `--debug` form from the pre-prototype state.
- [ ] `cargo test --workspace` passes.
- [ ] `./deploy.sh` produces a working CLAP whose editor is
      entirely HTML.
- [ ] Memory cleanup: archive the four Vizia-specific bug memories
      (`vxn1-vizia-no-click-slop`,
      `vxn1-vizia-automation-relayout-input-stomp`,
      `vxn1-vizia-absolute-stretch-overlay`,
      `vxn1-vizia-layout-probe-tool`) with a leading note that they
      describe the retired editor. Don't delete — they're history.

## Notes

This ticket is the cleanup at the end of E011. Land it only after
0048–0053 are all in `closed/` and the new HTML faceplate has had
some real usage time.

The `layout-probe` feature + the `Probe` machinery in vxn-ui-vizia
were instrumentation for *measuring* the Vizia editor; after deletion
it carries nothing useful. `target/vxn-layout.jsonl` (the dump it
produced) stays in the repo as a layout reference — committed to the
repo or to the ADR alongside the new HTML, designer's choice.

After this ticket, the dependency graph is:
`vxn-dsp → vxn-engine → vxn-app → vxn-ui-web` plus `vxn-clap`
depending on app + ui-web. Five crates plus xtask. Clean.
