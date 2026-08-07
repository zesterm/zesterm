//! Windows-specific window setup.

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
