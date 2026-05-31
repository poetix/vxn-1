//! Pluggable editor backend (ADR 0007 §2, §4).
//!
//! The clack shell talks only through this trait; whichever editor crate is
//! compiled in (`vxn-ui-vizia` today, `vxn-ui-web` after E010) provides the
//! impl. Parent-window type is associated so this crate stays free of any
//! windowing dependency.

use crate::controller::{ControllerHandle, CorpusHandle};
use crate::events::ViewEvent;

pub trait EditorBackend: 'static {
    /// Concrete handle returned by [`Self::open`] — the host keeps this alive
    /// for the editor's lifetime.
    type Handle;

    /// Backend-specific parent window descriptor (raw window handle for
    /// Vizia/baseview; an `NSView` pointer for the WebView crate).
    type ParentWindow;

    /// `corpus` is the controller-published preset snapshot. The backend
    /// reads it on open to seed its browser panel and re-reads after each
    /// [`ViewEvent::PresetCorpusChanged`].
    fn open(
        parent: Self::ParentWindow,
        ctrl: ControllerHandle,
        corpus: CorpusHandle,
    ) -> Self::Handle;
    fn close(handle: &mut Self::Handle);

    /// Forward a `ViewEvent` into the backend's render context. Called from
    /// the controller's thread; the backend is responsible for marshalling
    /// onto its own UI thread if needed. Backends that batch (the WebView
    /// IPC bridge, where each `evaluate_script` is a non-trivial JS context
    /// crossing) should buffer here and flush via [`Self::flush_view_events`]
    /// at the end of the host's tick.
    fn push_view_event(handle: &Self::Handle, event: ViewEvent);

    /// Flush any events buffered by [`Self::push_view_event`]. Called once
    /// per host tick after every push. Default is a no-op — backends that
    /// dispatch synchronously (Vizia's on_idle) ignore it.
    fn flush_view_events(_handle: &Self::Handle) {}
}
