//! Floating NSWindow text-input popup (0048 / E011).
//!
//! Hosts swallow Space and friends for transport before any child NSView
//! sees them, so the HTML faceplate can't host a text field directly.
//! Workaround: a borderless NSWindow that becomes its own key window —
//! outside the host's NSEvent monitor scope. Standard trick across
//! Spitfire / Output / Arturia for preset rename. Used by the rename /
//! save-as / new-folder flows in 0049 / 0051.
//!
//! macOS only for now. Windows + Linux equivalents are deferred to
//! per-platform tickets; this module ships a stub that immediately
//! cancels on those targets so the originating JS callback fires with
//! `None` instead of hanging.

/// Boxed one-shot result handler. Internal alias used by the macOS popup
/// module's ivar storage; callers go through [`prompt_text`].
type Callback = Box<dyn FnOnce(Option<String>) + Send + 'static>;

/// Spawn the floating text-input popup over `parent` and fire `callback`
/// exactly once: `Some(value)` on Enter, `None` on Esc / click outside.
/// macOS + Windows; Linux falls through to a stub that cancels
/// synchronously (so the originating JS callback fires with `None`
/// instead of hanging) until a GTK-side popup lands.
///
/// `parent` is the host's native parent window — NSView on macOS, HWND
/// on Windows. The popup centres over its window; may be null when the
/// editor hasn't been attached yet (falls back to screen centre).
pub fn prompt_text<F>(parent: *mut std::ffi::c_void, title: &str, initial: &str, callback: F)
where
    F: FnOnce(Option<String>) + Send + 'static,
{
    let boxed: Callback = Box::new(callback);
    #[cfg(target_os = "macos")]
    {
        macos::open_popup(parent, title, initial, boxed);
    }
    #[cfg(target_os = "windows")]
    {
        win32::open_popup(parent, title, initial, boxed);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (parent, title, initial);
        // Stub: immediately cancel so the JS pending-callback map
        // doesn't leak waiting on a popup that will never appear.
        // Linux (GTK) equivalent lands in a follow-up ticket.
        boxed(None);
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_op_in_unsafe_fn)]
mod macos {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Once;

    use objc::declare::ClassDecl;
    use objc::runtime::{BOOL, Class, NO, Object, Sel, YES};
    use objc::{Encode, Encoding, class, msg_send, sel, sel_impl};

    use super::Callback;

    // ── NSGeometry types ───────────────────────────────────────────────────
    //
    // objc 0.2 doesn't ship Encode impls for Cocoa structs. Roll them locally
    // so we don't pull cocoa-foundation in just for two msg_send arg types.

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct NSPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct NSRect {
        origin: NSPoint,
        size: NSSize,
    }

    unsafe impl Encode for NSPoint {
        fn encode() -> Encoding {
            unsafe { Encoding::from_str("{CGPoint=dd}") }
        }
    }
    unsafe impl Encode for NSSize {
        fn encode() -> Encoding {
            unsafe { Encoding::from_str("{CGSize=dd}") }
        }
    }
    unsafe impl Encode for NSRect {
        fn encode() -> Encoding {
            unsafe { Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
        }
    }

    // ── Popup geometry ─────────────────────────────────────────────────────

    const POPUP_WIDTH: f64 = 320.0;
    const POPUP_HEIGHT: f64 = 78.0;
    const PADDING: f64 = 12.0;
    const TITLE_HEIGHT: f64 = 16.0;
    const FIELD_HEIGHT: f64 = 24.0;

    // ── Cocoa constants ────────────────────────────────────────────────────

    const NS_WINDOW_STYLE_MASK_BORDERLESS: u64 = 0;
    const NS_BACKING_STORE_BUFFERED: u64 = 2;
    const NS_FLOATING_WINDOW_LEVEL: i64 = 3;
    const NS_FOCUS_RING_TYPE_NONE: u64 = 1;

    // ── Ivars on the VxnPromptWindow subclass ──────────────────────────────

    const IVAR_CALLBACK: &str = "_vxnCallback";
    const IVAR_FIELD: &str = "_vxnField";

    static REGISTER: Once = Once::new();

    /// Lazily declare `VxnPromptWindow : NSWindow` — adds the
    /// ivars + the three Sel handlers (commit, cancel, resign-key) and
    /// overrides `canBecomeKeyWindow` so a borderless window can claim the
    /// keyboard. Called once per process; subsequent calls reuse the
    /// registered class.
    unsafe fn ensure_class() -> &'static Class {
        REGISTER.call_once(|| {
            let superclass = class!(NSWindow);
            let mut decl = ClassDecl::new("VxnPromptWindow", superclass)
                .expect("declare VxnPromptWindow");
            decl.add_ivar::<*mut c_void>(IVAR_CALLBACK);
            decl.add_ivar::<*mut c_void>(IVAR_FIELD);
            decl.add_method(
                sel!(vxnCommit:),
                commit_action as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(cancelOperation:),
                cancel_action as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(vxnResignKey:),
                resign_key as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(canBecomeKeyWindow),
                can_become_key as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.register();
        });
        Class::get("VxnPromptWindow").expect("VxnPromptWindow registered")
    }

    extern "C" fn can_become_key(_this: &Object, _sel: Sel) -> BOOL {
        // Borderless windows default to NO; without this Enter fires no
        // action and the field can't take focus.
        YES
    }

    extern "C" fn commit_action(this: &mut Object, _sel: Sel, _sender: *mut Object) {
        unsafe {
            let Some(cb) = take_callback(this) else { return };
            let field_ptr: *mut c_void = *this.get_ivar(IVAR_FIELD);
            let field = field_ptr as *mut Object;
            let nsstr: *mut Object = msg_send![field, stringValue];
            let value = ns_string_to_rust(nsstr);
            dismiss(this);
            (cb)(Some(value));
        }
    }

    extern "C" fn cancel_action(this: &mut Object, _sel: Sel, _sender: *mut Object) {
        unsafe {
            let Some(cb) = take_callback(this) else { return };
            dismiss(this);
            (cb)(None);
        }
    }

    extern "C" fn resign_key(this: &mut Object, _sel: Sel, _note: *mut Object) {
        // Losing key window status == click outside. Same path as Esc.
        unsafe {
            let Some(cb) = take_callback(this) else { return };
            dismiss(this);
            (cb)(None);
        }
    }

    /// Take the boxed callback out of the window's ivar. Returns `None`
    /// if it's already been taken — guarantees the closure fires exactly
    /// once across commit / cancel / resign-key.
    unsafe fn take_callback(this: &mut Object) -> Option<Callback> {
        let cb_ptr: *mut c_void = *this.get_ivar(IVAR_CALLBACK);
        if cb_ptr.is_null() {
            return None;
        }
        this.set_ivar::<*mut c_void>(IVAR_CALLBACK, ptr::null_mut());
        // Double-box: the outer `Box<Callback>` is thin, so we can
        // round-trip it through a `*mut c_void` ivar without losing the
        // inner trait object's vtable.
        let outer: Box<Callback> = Box::from_raw(cb_ptr as *mut Callback);
        Some(*outer)
    }

    /// Tear the window down: remove the resign-key observer, order it
    /// out, release. `setReleasedWhenClosed:NO` keeps Cocoa from
    /// double-releasing under us, so `[release]` here is the only drop.
    unsafe fn dismiss(this: &mut Object) {
        // Drop the &mut down to a raw pointer once so `msg_send!` can
        // pass it by value to multiple selectors without moving the
        // borrow on each call.
        let this_ptr: *mut Object = this;
        let center: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![center, removeObserver: this_ptr];
        let _: () = msg_send![this_ptr, orderOut: ptr::null_mut::<Object>()];
        let _: () = msg_send![this_ptr, release];
    }

    /// Crate-internal entry called by [`super::prompt_text`]. Takes the
    /// callback pre-boxed so the macOS subclass can stash it in an
    /// `*mut c_void` ivar without round-tripping through a generic
    /// monomorph.
    pub(super) fn open_popup(parent: *mut c_void, title: &str, initial: &str, callback: Callback) {
        unsafe {
            let cls = ensure_class();
            let rect = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize { width: POPUP_WIDTH, height: POPUP_HEIGHT },
            };
            let window: *mut Object = msg_send![cls, alloc];
            let window: *mut Object = msg_send![
                window,
                initWithContentRect: rect
                styleMask: NS_WINDOW_STYLE_MASK_BORDERLESS
                backing: NS_BACKING_STORE_BUFFERED
                defer: NO
            ];
            let _: () = msg_send![window, setReleasedWhenClosed: NO];
            let _: () = msg_send![window, setLevel: NS_FLOATING_WINDOW_LEVEL];
            let _: () = msg_send![window, setHasShadow: YES];
            let bg: *mut Object = msg_send![
                class!(NSColor),
                colorWithCalibratedRed: 0.10_f64
                green: 0.10_f64
                blue: 0.12_f64
                alpha: 1.0_f64
            ];
            let _: () = msg_send![window, setBackgroundColor: bg];

            // Title label across the top.
            let label_rect = NSRect {
                origin: NSPoint { x: PADDING, y: POPUP_HEIGHT - PADDING - TITLE_HEIGHT },
                size: NSSize { width: POPUP_WIDTH - 2.0 * PADDING, height: TITLE_HEIGHT },
            };
            let label_cls = class!(NSTextField);
            let label: *mut Object = msg_send![label_cls, alloc];
            let label: *mut Object = msg_send![label, initWithFrame: label_rect];
            let _: () = msg_send![label, setStringValue: ns_string(title)];
            let _: () = msg_send![label, setEditable: NO];
            let _: () = msg_send![label, setSelectable: NO];
            let _: () = msg_send![label, setBezeled: NO];
            let _: () = msg_send![label, setBordered: NO];
            let _: () = msg_send![label, setDrawsBackground: NO];
            let title_color: *mut Object =
                msg_send![class!(NSColor), colorWithCalibratedWhite: 0.85_f64 alpha: 1.0_f64];
            let _: () = msg_send![label, setTextColor: title_color];

            // Text field across the bottom; Enter sends -vxnCommit: to
            // the window.
            let field_rect = NSRect {
                origin: NSPoint { x: PADDING, y: PADDING },
                size: NSSize { width: POPUP_WIDTH - 2.0 * PADDING, height: FIELD_HEIGHT },
            };
            let field_cls = class!(NSTextField);
            let field: *mut Object = msg_send![field_cls, alloc];
            let field: *mut Object = msg_send![field, initWithFrame: field_rect];
            let _: () = msg_send![field, setStringValue: ns_string(initial)];
            let _: () = msg_send![field, setEditable: YES];
            let _: () = msg_send![field, setSelectable: YES];
            let _: () = msg_send![field, setBezeled: YES];
            let _: () = msg_send![field, setBordered: YES];
            let _: () = msg_send![field, setDrawsBackground: YES];
            let _: () = msg_send![field, setFocusRingType: NS_FOCUS_RING_TYPE_NONE];
            let _: () = msg_send![field, setTarget: window];
            let _: () = msg_send![field, setAction: sel!(vxnCommit:)];

            // Mount both subviews on the content view.
            let content: *mut Object = msg_send![window, contentView];
            let _: () = msg_send![content, addSubview: label];
            let _: () = msg_send![content, addSubview: field];

            // Stash the boxed callback + the field for commit-time read.
            let boxed_callback: Box<Callback> = Box::new(callback);
            let cb_ptr = Box::into_raw(boxed_callback) as *mut c_void;
            let window_obj: &mut Object = &mut *window;
            window_obj.set_ivar::<*mut c_void>(IVAR_CALLBACK, cb_ptr);
            window_obj.set_ivar::<*mut c_void>(IVAR_FIELD, field as *mut c_void);

            // Position over the parent NSView's window, falling back to
            // screen-center when the parent is detached or null.
            position_over_parent(window, parent as *mut Object);

            // Click-outside cancels via NSWindowDidResignKey: register
            // self as both observer and target so removeObserver: on
            // dismiss is a single call.
            let center: *mut Object =
                msg_send![class!(NSNotificationCenter), defaultCenter];
            let name = ns_string("NSWindowDidResignKeyNotification");
            let _: () = msg_send![
                center,
                addObserver: window
                selector: sel!(vxnResignKey:)
                name: name
                object: window
            ];

            let _: () = msg_send![window, makeKeyAndOrderFront: ptr::null_mut::<Object>()];
            let _: () = msg_send![window, makeFirstResponder: field];
            // Select all so the user can type-replace immediately
            // (matches Finder rename, the muscle-memory baseline).
            let _: () = msg_send![field, selectText: ptr::null_mut::<Object>()];
        }
    }

    /// Centre `window` over `parent`'s screen rectangle. Falls back to
    /// `[NSWindow center]` (main-screen centre) when the parent has no
    /// attached window — e.g. just after the host attaches the editor.
    unsafe fn position_over_parent(window: *mut Object, parent_view: *mut Object) {
        if parent_view.is_null() {
            let _: () = msg_send![window, center];
            return;
        }
        let parent_window: *mut Object = msg_send![parent_view, window];
        if parent_window.is_null() {
            let _: () = msg_send![window, center];
            return;
        }
        let parent_frame: NSRect = msg_send![parent_window, frame];
        let cx = parent_frame.origin.x + parent_frame.size.width * 0.5;
        let cy = parent_frame.origin.y + parent_frame.size.height * 0.5;
        let origin = NSPoint {
            x: cx - POPUP_WIDTH * 0.5,
            y: cy - POPUP_HEIGHT * 0.5,
        };
        let _: () = msg_send![window, setFrameOrigin: origin];
    }

    /// Build an autoreleased `NSString` from a Rust `&str`. Internal nulls
    /// would terminate the C string early — strip them rather than fail.
    unsafe fn ns_string(s: &str) -> *mut Object {
        let mut bytes: Vec<u8> = s.bytes().filter(|&b| b != 0).collect();
        bytes.push(0);
        let cstr = bytes.as_ptr() as *const i8;
        msg_send![class!(NSString), stringWithUTF8String: cstr]
    }

    /// Pull a Rust `String` out of an NSString. Empty / null in →
    /// empty out; invalid UTF-8 (Cocoa shouldn't produce any from a
    /// text-field's UTF-8 accessor) is replaced lossily.
    unsafe fn ns_string_to_rust(s: *mut Object) -> String {
        if s.is_null() {
            return String::new();
        }
        let utf8: *const i8 = msg_send![s, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }

}

#[cfg(target_os = "windows")]
#[allow(unsafe_op_in_unsafe_fn)]
mod win32 {
    //! Windows side of the 0048 popup. Mirrors the macOS module: an
    //! owned WS_POPUP window with a child EDIT control whose WndProc
    //! we subclass to grab VK_RETURN / VK_ESCAPE before the host's
    //! accelerator table sees them. `WM_ACTIVATE(WA_INACTIVE)` on the
    //! popup is the click-outside-cancels trigger, guarded by a
    //! `has_activated` flag so the brief inactive flicker during
    //! construction doesn't immediately dismiss us.

    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SetFocus, VK_ESCAPE, VK_RETURN,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, CreateWindowExW, CS_HREDRAW, CS_VREDRAW, DLGC_WANTALLKEYS,
        DefWindowProcW, DestroyWindow, ES_AUTOHSCROLL, GWLP_USERDATA, GWLP_WNDPROC,
        GetParent, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, IDC_ARROW, LoadCursorW, RegisterClassExW, SendMessageW,
        SetForegroundWindow, SetWindowLongPtrW, WA_ACTIVE, WA_CLICKACTIVE,
        WA_INACTIVE, WM_ACTIVATE, WM_GETDLGCODE, WM_KEYDOWN, WM_NCDESTROY,
        WNDCLASSEXW, WNDPROC, WS_BORDER, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_POPUP, WS_VISIBLE,
    };

    // `EM_SETSEL` lives under `UI::Controls` and `SS_LEFT` under
    // `System::SystemServices` in windows-sys 0.61 — both modules carry
    // their own feature flags that this crate doesn't need otherwise.
    // The numeric values are stable Win32 constants; pin them as literals
    // to keep the dep surface tight.
    const EM_SETSEL: u32 = 0x00B1;
    const SS_LEFT: u32 = 0;

    use super::Callback;

    // Popup geometry — matches the macOS module so a faceplate that
    // promptText()s at the same logical size lands a similarly-sized
    // window on either platform.
    const POPUP_WIDTH: i32 = 320;
    const POPUP_HEIGHT: i32 = 84;
    const PADDING: i32 = 12;
    const TITLE_HEIGHT: i32 = 16;
    const FIELD_HEIGHT: i32 = 24;
    const GAP: i32 = 6;

    static CLASS_REGISTER: Once = Once::new();

    // Class name + built-in EDIT / STATIC names, all UTF-16 null-terminated
    // so we can pass `.as_ptr()` straight to Win32.
    static CLASS_NAME: &[u16] = &[
        b'V' as u16, b'x' as u16, b'n' as u16, b'P' as u16, b'r' as u16, b'o' as u16,
        b'm' as u16, b'p' as u16, b't' as u16, b'W' as u16, b'i' as u16, b'n' as u16, 0,
    ];
    static EDIT_CLASS: &[u16] = &[b'E' as u16, b'D' as u16, b'I' as u16, b'T' as u16, 0];
    static STATIC_CLASS: &[u16] = &[
        b'S' as u16, b'T' as u16, b'A' as u16, b'T' as u16, b'I' as u16, b'C' as u16, 0,
    ];

    /// Per-popup state. The Box's raw pointer rides in both the popup's
    /// and edit's `GWLP_USERDATA`; the popup's `WM_NCDESTROY` owns the
    /// `Box::from_raw` drop. `callback.take()` is the fire-once gate.
    struct PopupState {
        callback: Option<Callback>,
        orig_edit_proc: isize,
        /// `true` once the popup has received its first `WA_ACTIVE` /
        /// `WA_CLICKACTIVE` — guards against the brief construction-time
        /// inactive flicker masquerading as a click-outside dismiss.
        has_activated: bool,
    }

    unsafe fn ensure_class() {
        CLASS_REGISTER.call_once(|| unsafe {
            let hinstance = GetModuleHandleW(ptr::null());
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(popup_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: ptr::null_mut(),
                hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                hbrBackground: ((COLOR_BTNFACE as usize) + 1) as HBRUSH,
                lpszMenuName: ptr::null(),
                lpszClassName: CLASS_NAME.as_ptr(),
                hIconSm: ptr::null_mut(),
            };
            RegisterClassExW(&wc);
        });
    }

    pub(super) fn open_popup(
        parent: *mut c_void,
        title: &str,
        initial: &str,
        callback: Callback,
    ) {
        unsafe {
            open_popup_inner(parent as HWND, title, initial, callback);
        }
    }

    unsafe fn open_popup_inner(
        parent: HWND,
        title: &str,
        initial: &str,
        callback: Callback,
    ) {
        ensure_class();
        let hinstance = GetModuleHandleW(ptr::null());

        let (x, y) = popup_origin(parent);

        // Window title doubles as the accessibility label; the visible
        // label is the child STATIC below.
        let title_w = to_wide(title);
        let popup = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME.as_ptr(),
            title_w.as_ptr(),
            WS_POPUP | WS_BORDER | WS_VISIBLE,
            x,
            y,
            POPUP_WIDTH,
            POPUP_HEIGHT,
            parent,
            ptr::null_mut(),
            hinstance,
            ptr::null_mut(),
        );
        if popup.is_null() {
            // RegisterClassExW/CreateWindowExW failure: surface as cancel
            // so the JS callback still fires.
            callback(None);
            return;
        }

        let label_w = to_wide(title);
        let _label = CreateWindowExW(
            0,
            STATIC_CLASS.as_ptr(),
            label_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            PADDING,
            PADDING,
            POPUP_WIDTH - 2 * PADDING,
            TITLE_HEIGHT,
            popup,
            ptr::null_mut(),
            hinstance,
            ptr::null_mut(),
        );

        let initial_w = to_wide(initial);
        let edit = CreateWindowExW(
            0,
            EDIT_CLASS.as_ptr(),
            initial_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32,
            PADDING,
            PADDING + TITLE_HEIGHT + GAP,
            POPUP_WIDTH - 2 * PADDING,
            FIELD_HEIGHT,
            popup,
            ptr::null_mut(),
            hinstance,
            ptr::null_mut(),
        );

        // Boxed state stowed in GWLP_USERDATA on both windows. The
        // popup's WM_NCDESTROY owns the eventual drop — edit's GWLP is
        // for the subclass to find the shared state without round-trip
        // through GetParent().
        let state = Box::into_raw(Box::new(PopupState {
            callback: Some(callback),
            orig_edit_proc: 0,
            has_activated: false,
        }));
        SetWindowLongPtrW(popup, GWLP_USERDATA, state as isize);

        // Subclass the EDIT WndProc. The original returns as the value
        // we'll pass to CallWindowProcW for unhandled msgs.
        let orig = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            edit_subclass_proc as *const () as isize,
        );
        (*state).orig_edit_proc = orig;
        SetWindowLongPtrW(edit, GWLP_USERDATA, state as isize);

        // Select all so the user can type-replace (Finder-rename muscle
        // memory — wparam=0/lparam=-1 means "whole range").
        SendMessageW(edit, EM_SETSEL, 0, -1);
        SetFocus(edit);
        SetForegroundWindow(popup);
    }

    unsafe fn popup_origin(parent: HWND) -> (i32, i32) {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if !parent.is_null() && GetWindowRect(parent, &mut rect) != 0 {
            let cx = (rect.left + rect.right) / 2;
            let cy = (rect.top + rect.bottom) / 2;
            (cx - POPUP_WIDTH / 2, cy - POPUP_HEIGHT / 2)
        } else {
            // No parent geometry — drop the popup at a safe offset
            // from the top-left of the primary monitor.
            (100, 100)
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        // Strip embedded NULs (would terminate the C string early) and
        // append the trailing NUL Win32 expects.
        let mut v: Vec<u16> = s.encode_utf16().filter(|&c| c != 0).collect();
        v.push(0);
        v
    }

    unsafe extern "system" fn popup_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_ACTIVATE => {
                let activation = (wparam as u32) & 0xFFFF;
                let state_ptr =
                    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
                if state_ptr.is_null() {
                    return 0;
                }
                let state = &mut *state_ptr;
                if activation == WA_ACTIVE || activation == WA_CLICKACTIVE {
                    state.has_activated = true;
                } else if activation == WA_INACTIVE && state.has_activated {
                    if let Some(cb) = state.callback.take() {
                        DestroyWindow(hwnd);
                        cb(None);
                    }
                }
                0
            }
            WM_NCDESTROY => {
                // Last message — children are gone by now. If commit /
                // cancel already took the callback, this just drops the
                // box; otherwise treat the disappearance as a cancel
                // (Alt+F4, host tear-down).
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let mut state: Box<PopupState> = Box::from_raw(ptr);
                    if let Some(cb) = state.callback.take() {
                        cb(None);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe extern "system" fn edit_subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;

        // EDIT defers Enter / Esc to a dialog manager by default; we
        // aren't using one, so claim them with DLGC_WANTALLKEYS and
        // handle in WM_KEYDOWN below.
        if msg == WM_GETDLGCODE {
            return DLGC_WANTALLKEYS as LRESULT;
        }

        if msg == WM_KEYDOWN {
            let vk = wparam as u32;
            if vk == VK_RETURN as u32 {
                commit(hwnd, state_ptr);
                return 0;
            }
            if vk == VK_ESCAPE as u32 {
                cancel(hwnd, state_ptr);
                return 0;
            }
        }

        let orig = if state_ptr.is_null() {
            0
        } else {
            (*state_ptr).orig_edit_proc
        };
        if orig != 0 {
            let proc: WNDPROC = std::mem::transmute(orig);
            CallWindowProcW(proc, hwnd, msg, wparam, lparam)
        } else {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    unsafe fn commit(edit: HWND, state_ptr: *mut PopupState) {
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        let Some(cb) = state.callback.take() else { return };
        let text = read_edit_text(edit);
        let popup = GetParent(edit);
        DestroyWindow(popup);
        cb(Some(text));
    }

    unsafe fn cancel(edit: HWND, state_ptr: *mut PopupState) {
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        let Some(cb) = state.callback.take() else { return };
        let popup = GetParent(edit);
        DestroyWindow(popup);
        cb(None);
    }

    unsafe fn read_edit_text(edit: HWND) -> String {
        let len = GetWindowTextLengthW(edit);
        if len <= 0 {
            return String::new();
        }
        let mut buf: Vec<u16> = vec![0u16; (len + 1) as usize];
        let read = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
        if read <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..read as usize])
    }
}
