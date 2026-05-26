//! CLAP `gui` extension: embeds the [`vxn_ui`] Vizia editor into the host's
//! parent window. The editor talks to the engine purely through the shared
//! parameter store (`vxn_engine::SharedParams`); see [`crate::local`] for how UI
//! edits are echoed to the host.

use crate::VxnMainThread;
use clack_extensions::gui::*;
use clack_plugin::prelude::*;
use std::sync::Arc;

/// Backing scale factor of the host's parent NSView, via its window (falling
/// back to the main screen when the view isn't in a window yet). Used to pin the
/// editor's HiDPI scale at attach time, since vizia's `SystemScaleFactor`
/// placeholder isn't reliably corrected after attach — see [`vxn_ui::open_editor`].
#[cfg(target_os = "macos")]
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
        if let Some(mut handle) = self.gui.take() {
            handle.close();
        }
    }

    fn set_scale(&mut self, _scale: f64) -> Result<(), PluginError> {
        Ok(())
    }

    fn get_size(&mut self) -> Option<GuiSize> {
        Some(GuiSize {
            width: vxn_ui::EDITOR_WIDTH,
            height: vxn_ui::EDITOR_HEIGHT,
        })
    }

    fn set_size(&mut self, _size: GuiSize) -> Result<(), PluginError> {
        // Fixed-size editor for now; accept whatever the host asks.
        Ok(())
    }

    fn set_parent(&mut self, window: Window) -> Result<(), PluginError> {
        // The host hands us its native parent window for the current platform's
        // GUI API (gated by `is_api_supported`/`get_preferred_api`). Pull out the
        // raw handle pointer per platform; `vxn_ui::open_editor` wraps it in
        // vizia's `ParentWindow`, which rebuilds the matching raw-window-handle
        // for the same OS. Without the per-OS branch the accessor returns `None`
        // off-macOS, so the editor never opens (the Windows "no UI" bug).
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

        // Open with baseview's default `WindowScalePolicy::SystemScaleFactor`.
        // The HiDPI factor is resolved when our NSView is added to the host
        // window (`addSubview` inside `open_parented`): that fires
        // `viewDidChangeBackingProperties`, whose size change drives a `Resized`
        // event that rebuilds vizia's Skia surface against the now-live backing
        // store. Pinning an explicit `ScaleFactor` here breaks that — it makes
        // the pre- and post-attach physical sizes equal, so the rebuild never
        // fires and the editor renders 1× into a 2× surface (bottom-left
        // quarter on a Retina Mac).
        // Pin the editor to the host window's real backing scale, read from the
        // parent NSView. vizia's `SystemScaleFactor` placeholder isn't corrected
        // on displays where the backing scale never changes after attach, so the
        // editor would otherwise render oversized — see `vxn_ui::open_editor`.
        #[cfg(target_os = "macos")]
        let scale_override = Some(parent_backing_scale(parent)).filter(|s| *s > 0.0);
        #[cfg(not(target_os = "macos"))]
        let scale_override = None;

        self.gui = Some(vxn_ui::open_editor(
            parent,
            Arc::clone(&self.shared.params),
            scale_override,
        ));
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
