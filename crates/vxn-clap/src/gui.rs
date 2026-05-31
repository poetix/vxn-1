//! CLAP `gui` extension: embeds whichever editor backend the build picked
//! (vizia or webview) into the host's parent window. The editor talks to the
//! engine through the controller (ADR 0007) — host echoes still go via
//! [`crate::local`]. Backend selection happens in [`crate::lib`]'s top-level
//! `vxn_editor` re-alias; this file only deals with the parent-window
//! plumbing and the per-backend `open_editor` call shapes.

use crate::{VxnMainThread, vxn_editor};
use clack_extensions::gui::*;
#[cfg(feature = "webview")]
use clack_extensions::timer::HostTimer;
use clack_plugin::prelude::*;
use std::sync::Arc;
#[cfg(feature = "vizia")]
use vxn_app::ParamModel;

/// 16 ms ≈ 60 Hz — fast enough to feel responsive on automation echo, slow
/// enough that hosts won't clamp it. CLAP spec asks hosts to support at least
/// 30 Hz, so this stays inside the supported envelope.
#[cfg(feature = "webview")]
const WEBVIEW_TIMER_PERIOD_MS: u32 = 16;

/// Backing scale factor of the host's parent NSView, via its window (falling
/// back to the main screen when the view isn't in a window yet). Used to pin
/// the vizia editor's HiDPI scale at attach time, since vizia's
/// `SystemScaleFactor` placeholder isn't reliably corrected after attach. The
/// webview backend uses CSS logical pixels and ignores this.
#[cfg(all(target_os = "macos", feature = "vizia"))]
fn parent_backing_scale(nsview: *mut std::ffi::c_void) -> f64 {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    if nsview.is_null() {
        return -1.0;
    }
    unsafe {
        let view = nsview as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if !window.is_null() {
            return msg_send![window, backingScaleFactor];
        }
        let cls = objc::runtime::Class::get("NSScreen");
        if let Some(cls) = cls {
            let screen: *mut Object = msg_send![cls, mainScreen];
            if !screen.is_null() {
                return msg_send![screen, backingScaleFactor];
            }
        }
        0.0
    }
}

impl PluginGuiImpl for VxnMainThread<'_> {
    fn is_api_supported(&mut self, config: GuiConfiguration) -> bool {
        Some(config.api_type) == GuiApiType::default_for_current_platform() && !config.is_floating
    }

    fn get_preferred_api(&mut self) -> Option<GuiConfiguration<'_>> {
        Some(GuiConfiguration {
            api_type: GuiApiType::default_for_current_platform()?,
            is_floating: false,
        })
    }

    fn create(&mut self, config: GuiConfiguration) -> Result<(), PluginError> {
        if config.is_floating || Some(config.api_type) != GuiApiType::default_for_current_platform()
        {
            return Err(PluginError::Message("Unsupported GUI configuration"));
        }
        Ok(())
    }

    fn destroy(&mut self) {
        #[cfg(feature = "webview")]
        if let Some((host_timer, id)) = self.timer.take() {
            // Best-effort: a host that lost track of the timer between
            // register and unregister isn't worth a panic — the editor is
            // tearing down anyway.
            let _ = host_timer.unregister_timer(&mut self.host, id);
        }
        if let Some(mut handle) = self.gui.take() {
            handle.close();
        }
    }

    fn set_scale(&mut self, _scale: f64) -> Result<(), PluginError> {
        Ok(())
    }

    fn get_size(&mut self) -> Option<GuiSize> {
        Some(GuiSize {
            width: vxn_editor::EDITOR_WIDTH,
            height: vxn_editor::EDITOR_HEIGHT,
        })
    }

    fn set_size(&mut self, _size: GuiSize) -> Result<(), PluginError> {
        // Fixed-size editor for now; accept whatever the host asks.
        Ok(())
    }

    fn set_parent(&mut self, window: Window) -> Result<(), PluginError> {
        // The host hands us its native parent window for the current
        // platform's GUI API (gated by `is_api_supported`/`get_preferred_api`).
        // Pull the raw pointer per platform; each backend wraps it in its own
        // window-handle shape inside `open_editor`. Without the per-OS branch
        // the accessor returns `None` off-macOS, so the editor never opens
        // (the Windows "no UI" bug).
        #[cfg(target_os = "macos")]
        let parent = window.as_cocoa_nsview().ok_or(PluginError::Message(
            "Expected a Cocoa (NSView) parent window",
        ))?;
        #[cfg(target_os = "windows")]
        let parent = window.as_win32_hwnd().ok_or(PluginError::Message(
            "Expected a Win32 (HWND) parent window",
        ))?;
        #[cfg(target_os = "linux")]
        let parent = window
            .as_x11_handle()
            .map(|h| h as *mut std::ffi::c_void)
            .ok_or(PluginError::Message("Expected an X11 parent window"))?;

        #[cfg(feature = "vizia")]
        {
            // Pin the editor to the host window's real backing scale, read from
            // the parent NSView. vizia's `SystemScaleFactor` placeholder isn't
            // corrected on displays where the backing scale never changes after
            // attach, so the editor would otherwise render oversized.
            #[cfg(target_os = "macos")]
            let scale_override = Some(parent_backing_scale(parent)).filter(|s| *s > 0.0);
            #[cfg(not(target_os = "macos"))]
            let scale_override = None;

            // Build a `ControllerHandle` for UiEvent posts. The view-event
            // receiver + corpus + tick come straight from the main thread; the
            // model is `SharedParams` erased to `dyn ParamModel` so the editor
            // never needs the engine type.
            let model: Arc<dyn ParamModel> = self.shared.params.clone();
            let handle = crate::lock_mut(&self.controller).handle();
            let view_rx = Arc::clone(&self.view_rx);
            let corpus = Arc::clone(&self.corpus);
            let tick = self.tick.clone();
            self.gui = Some(vxn_editor::open_editor(
                parent,
                model,
                handle,
                view_rx,
                corpus,
                tick,
                scale_override,
            ));
        }
        #[cfg(feature = "webview")]
        {
            // The webview backend takes the parent, a controller handle, and
            // the shared preset-corpus snapshot (0050) — the editor's browser
            // panel re-reads this on every `PresetCorpusChanged`. View-event
            // drain + controller tick are driven from the host's main-thread
            // timer (registered below), not an editor-internal idle hook.
            let ctrl_handle = crate::lock_mut(&self.controller).handle();
            let corpus = Arc::clone(&self.corpus);
            self.gui = Some(vxn_editor::open_editor(parent, ctrl_handle, corpus));

            // Register a periodic main-thread timer so `on_timer` can drain
            // ViewEvents into the WebView. Hosts without `timer-support`
            // leave the editor static — UI gestures still flow (they post
            // straight to the controller's channel), but DAW automation
            // won't echo to the page until a tick lands. We don't fail
            // GUI creation over it; it's a degraded mode, not a broken one.
            if let Some(host_timer) = self.host.shared().info().get_extension::<HostTimer>() {
                if let Ok(id) =
                    host_timer.register_timer(&mut self.host, WEBVIEW_TIMER_PERIOD_MS)
                {
                    self.timer = Some((host_timer, id));
                }
            }
        }
        Ok(())
    }

    fn set_transient(&mut self, _window: Window) -> Result<(), PluginError> {
        Ok(())
    }

    fn show(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn hide(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}
