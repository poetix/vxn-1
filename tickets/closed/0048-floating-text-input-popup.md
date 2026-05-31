---
id: "0048"
title: Floating NSWindow text-input popup (host kbd capture workaround)
priority: high
created: 2026-05-30
epic: E011
---

## Summary

Build a reusable floating NSWindow + NSTextField popup that the
WebView editor invokes for any text-input intent (preset rename,
save-as, new folder). The popup is its own key window — outside the
DAW's NSEvent monitor scope — so Space and friends behave as text
input rather than transport.

## Acceptance criteria

- [ ] `vxn-ui-web` exposes
      `fn prompt_text(parent: NSView, title: &str, initial: &str,
       callback: impl FnOnce(Option<String>) + Send + 'static)`.
- [ ] The popup is a borderless NSWindow with an NSTextField,
      positioned over the editor area, `becomeKeyWindow` claims the
      keyboard.
- [ ] Enter commits (`callback(Some(value))`); Esc cancels
      (`callback(None)`). Click outside the popup cancels.
- [ ] Spaces type as spaces. (Verify in Bitwig, Live, Logic, Reaper
      — the four DAWs known to grab Space for transport.)
- [ ] The WebView faceplate triggers the popup via a `UiEvent`
      variant — say `RequestTextInput { id, initial, kind }` — and
      the controller's tick relays the request to the editor
      backend, which opens the popup. On commit/cancel, the editor
      posts `UiEvent::TextInputResult { id, value }` back.
- [ ] macOS only for this ticket; the popup module lives behind
      `#[cfg(target_os = "macos")]`. Other platforms get a stub that
      returns `None` (no rename until Windows/Linux equivalents
      land).

## Notes

NSWindow setup pattern (in objc 0.2 — same crate vxn-clap already
uses):

```objc
NSWindow* w = [[NSWindow alloc] initWithContentRect:rect
                                         styleMask:NSWindowStyleMaskBorderless
                                           backing:NSBackingStoreBuffered
                                             defer:NO];
[w setLevel:NSFloatingWindowLevel];
[w makeKeyAndOrderFront:nil];
```

Field positioning: center over the parent NSView, ~280×24, dark theme
matching the faceplate. Style the popup so it reads as part of the
plugin, not a system alert.

The "click outside cancels" handler uses `NSEvent
addGlobalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown` —
remove on dismiss.

Why a popup and not just letting the host re-enable Space: because
no DAW lets us. Even DAWs with "send keys to plugin" toggles (Reaper)
require per-instance opt-in. Floating window dodges the question.
