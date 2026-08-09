//! Platform-specific window setup.

/// Give the window a solid background in the theme colour, painted by the OS.
///
/// # Why this is the actual startup fix
///
/// Bringing up a GPU is ~700ms of serial driver initialization before anything
/// can be presented — measured on this machine as 311ms for the Vulkan
/// instance, then adapter, device and swapchain; DX12 redistributes the cost but
/// totals the same. There is no other work to overlap it with, so a terminal
/// that waits for the GPU before showing a window cannot open in under about
/// three quarters of a second.
///
/// But a window does not need a GPU to be the right colour. Setting the window
/// class's background brush makes Windows erase it on the very first paint, so
/// the window can be shown immediately — correctly coloured — while the GPU
/// comes up behind it. When the first frame is finally presented it is the same
/// colour, so the handover is invisible.
///
/// This is also what keeps the fix robust: if GPU init is slow on some machine,
/// or a driver stalls, the window is still up and looks right.
#[cfg(windows)]
pub fn set_background_color(window: &winit::window::Window, r: u8, g: u8, b: u8) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetClassLongPtrW, GCLP_HBRBACKGROUND};

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return };
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;

    // COLORREF is 0x00BBGGRR, not RGB.
    let colorref = u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16);

    // SAFETY: hwnd is a live window we just created; CreateSolidBrush returns
    // an owned GDI object which the class takes over.
    unsafe {
        let brush = CreateSolidBrush(colorref);
        if brush.is_null() {
            return;
        }
        let previous = SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush as isize);
        // The class starts with no brush, so `previous` is normally 0. If a
        // brush was already installed -- a second window reusing the class --
        // release it rather than leaking a GDI handle per window.
        if previous != 0 {
            DeleteObject(previous as *mut core::ffi::c_void);
        }
    }
}

#[cfg(not(windows))]
pub fn set_background_color(_window: &winit::window::Window, _r: u8, _g: u8, _b: u8) {
    // X11 and Wayland have no equivalent that is worth the complexity: the
    // compositor does not paint an unmapped surface, so there is no white flash
    // to fix in the first place.
}

/// The traffic-light cluster's extent, in *logical* points: `(max_x, titlebar_height)`.
///
/// Asked of AppKit every time the chrome lays out, because the answer is not a
/// constant: the cluster moves with OS version and localization, and the
/// titlebar height changes with `fullsize_content_view`. Callers must treat
/// `None` as "no cluster to avoid" — which is also the fullscreen answer,
/// where the buttons auto-hide (the caller checks fullscreen; here `None`
/// just means the question could not be answered).
#[cfg(target_os = "macos")]
pub fn traffic_light_inset(window: &winit::window::Window) -> Option<(f64, f64)> {
    use objc2_app_kit::{NSView, NSWindow, NSWindowButton};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };

    // SAFETY: winit hands out the NSView it owns, and we are on the main
    // thread — every caller is a window-event handler, and AppKit delivers
    // those on the main thread by construction.
    let view = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    let ns_window: objc2::rc::Retained<NSWindow> = view.window()?;

    let zoom = ns_window.standardWindowButton(NSWindowButton::ZoomButton)?;
    let frame = zoom.frame();
    // SAFETY: the retained superview is read once, synchronously, on the main
    // thread; nothing deallocates the titlebar view while its window is alive
    // and being asked about.
    let bar =
        unsafe { zoom.superview() }.map_or(frame.size.height, |sv| sv.frame().size.height);

    // The rightmost button's right edge is where tabs may begin.
    Some((frame.origin.x + frame.size.width, bar))
}

#[cfg(not(target_os = "macos"))]
pub fn traffic_light_inset(_window: &winit::window::Window) -> Option<(f64, f64)> {
    None
}
