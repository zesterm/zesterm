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

/// Ask the compositor for a backdrop material behind the window.
///
/// Written against `DwmSetWindowAttribute` rather than winit's
/// `set_system_backdrop`, which wraps the same call: winit discards the
/// `HRESULT`, and `DWMWA_SYSTEMBACKDROP_TYPE` is Windows 11 22H2+ — so on
/// Windows 10 the setting would do nothing and say nothing, which is the
/// failure mode ADR-003 exists to forbid. Same constants as
/// `zest-render-wgpu`'s `alpha_probe`, which is where they were first proven.
///
/// **Mica is drawn *behind* the window**, so it is visible only through
/// pixels we leave transparent — that means `window.opacity < 1.0` *and* an
/// adapter that reports `PreMultiplied` composite alpha, which per ADR-003 is
/// Vulkan and not DX12. Setting a backdrop on an opaque window is legal and
/// invisible, and that is not this function's business to refuse.
#[cfg(windows)]
pub fn set_backdrop(window: &winit::window::Window, backdrop: zest_config::settings::Backdrop) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use zest_config::settings::Backdrop;

    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    // `None` maps to DWMSBT_NONE and *not* to DWMSBT_AUTO (0): auto lets DWM
    // pick, which would make "none" mean "whatever it feels like".
    let value: i32 = match backdrop {
        Backdrop::None => 1,
        Backdrop::Mica => 2,
        Backdrop::Acrylic => 3,
        Backdrop::MicaAlt => 4,
        // A macOS material. Nothing to ask DWM for, and warning about it on
        // every launch of a config shared between two machines would be noise.
        Backdrop::Vibrancy => return,
    };

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return };
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;

    // SAFETY: a live window we own, and a 4-byte value whose length we pass.
    let hr = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            std::ptr::addr_of!(value).cast(),
            size_of::<i32>() as u32,
        )
    };
    if hr != 0 {
        tracing::warn!(
            hr = format!("0x{hr:08x}"),
            "this Windows build has no system backdrop (needs 11 22H2+); window.backdrop ignored"
        );
    }
}

/// The `NSVisualEffectMaterial` a backdrop asks for on macOS, if any.
///
/// `None` means "this window has no macOS backdrop", which covers both
/// [`Backdrop::None`] and the three Windows materials — a config shared between
/// two machines names `mica` on both, and the honest answer here is no backdrop
/// rather than a guess at what Mica would have looked like.
///
/// Pure so the whole table is testable; the AppKit half below is what cannot be.
#[cfg(target_os = "macos")]
fn material_for(
    backdrop: zest_config::settings::Backdrop,
) -> Option<objc2_app_kit::NSVisualEffectMaterial> {
    use objc2_app_kit::NSVisualEffectMaterial;
    use zest_config::settings::Backdrop;
    match backdrop {
        Backdrop::None => None,
        // `UnderWindowBackground` is the material AppKit documents for content
        // sitting over the desktop, which is exactly a terminal with a
        // translucent grid. Chosen by eye against the shipped themes over
        // `HUDWindow` (too dark under `paper`) and `Sidebar` (too light under
        // `obsidian`); it is a look, not a correctness question, so the reason
        // it can be revisited is written here rather than argued from the
        // header. Deliberately not `AppearanceBased`, which is deprecated and
        // whose own header says to use a semantic value.
        Backdrop::Vibrancy => Some(NSVisualEffectMaterial::UnderWindowBackground),
        Backdrop::Mica | Backdrop::MicaAlt | Backdrop::Acrylic => None,
    }
}

/// Put an `NSVisualEffectView` behind the window, or take one away.
///
/// **Only ever visible through pixels the surface above leaves transparent**,
/// which on a Mac means `window.opacity < 1.0` *and* a swapchain that took a
/// transparent composite mode. Until #309 that second half never happened —
/// wgpu's Metal backend offers `PostMultiplied` and this app demanded
/// `PreMultiplied` — so vibrancy would have been invisible however correct this
/// function was. Setting a backdrop on an opaque window is legal and invisible,
/// and that is not this function's business to refuse.
///
/// The effect view is a sibling *behind* winit's view rather than a child of
/// it: winit's view hosts the `CAMetalLayer` the renderer draws into, so a
/// subview of it would composite on top of the terminal rather than under it.
#[cfg(target_os = "macos")]
pub fn set_backdrop(window: &winit::window::Window, backdrop: zest_config::settings::Backdrop) {
    use objc2::{ClassType, MainThreadMarker};
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectState,
        NSVisualEffectView, NSWindowOrderingMode,
    };
    use objc2_foundation::NSObjectProtocol;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let material = material_for(backdrop);
    if material.is_none() && backdrop != zest_config::settings::Backdrop::None {
        tracing::warn!(?backdrop, "that backdrop is a Windows material; macOS has none of it");
    }

    // Every caller is a window-event handler, which AppKit delivers on the main
    // thread — but this asks rather than asserting, because the cost of being
    // wrong is UB and the cost of being careful is one branch.
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("window.backdrop needs the main thread; ignoring");
        return;
    };
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return };

    // SAFETY: winit hands out the NSView it owns and we are on the main thread,
    // as the marker above establishes.
    let view = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
    // SAFETY: the superview is read once, synchronously, on the main thread;
    // nothing deallocates it while its window is alive and being asked about.
    let Some(container) = (unsafe { view.superview() }) else {
        tracing::warn!("no superview to put a backdrop behind; ignoring window.backdrop");
        return;
    };

    // Remove any effect view we added before, rather than tracking one in app
    // state. A `vibrancy` -> `none` change *must* take the old one away, and
    // rediscovering it here means there is no second place for that state to
    // get out of step -- and no way to leave a blur behind for ever.
    for sub in &container.subviews() {
        if sub.isKindOfClass(NSVisualEffectView::class()) {
            sub.removeFromSuperview();
        }
    }
    let Some(material) = material else { return };

    let effect = NSVisualEffectView::new(mtm);
    effect.setMaterial(material);
    // `BehindWindow` is the whole point: `WithinWindow` blurs what is already
    // drawn in this window, which for a terminal is its own text.
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // Not `FollowsWindowActiveState`, the default: a backdrop that greys out
    // the moment the window loses focus reads as a rendering bug, and a
    // terminal spends much of its life unfocused while something runs in it.
    effect.setState(NSVisualEffectState::Active);
    effect.setFrame(container.bounds());
    // Tracks the window through a drag with no per-resize call of our own.
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    // `Below` with no sibling puts this at the back of the container, behind
    // the view the renderer draws into.
    container.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, None);

    // What it actually attached to, at debug level. Whether a backdrop is
    // *visible* cannot be asserted from inside the process — it is the
    // compositor's answer, and a screenshot of our own texture cannot show it —
    // so "why is my backdrop invisible" is otherwise unanswerable without a
    // debugger. The container's class is the fact that distinguishes "attached
    // behind the renderer's view" from "attached somewhere that will never
    // show".
    tracing::debug!(
        container = %container.class().name().to_string_lossy(),
        siblings = container.subviews().len(),
        "vibrancy attached"
    );
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn set_backdrop(_window: &winit::window::Window, backdrop: zest_config::settings::Backdrop) {
    // Linux is WS-D, and `AGENTS.md` records why it may stay that way: blur has
    // no portable path -- X11/KWin has `_KDE_NET_WM_BLUR_BEHIND_REGION`, picom
    // needs user rules, Wayland has no protocol at all. Degrade honestly rather
    // than pretending in the settings UI.
    if backdrop != zest_config::settings::Backdrop::None {
        tracing::warn!(?backdrop, "window.backdrop is not implemented on this platform yet");
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
/// Whether the OS has been asked to reduce motion.
///
/// An accessibility setting, not a preference we own: vestibular disorders make
/// animated scrolling genuinely unpleasant, and `motion.respect_system_reduce_motion`
/// defaults to on for that reason. Read live on every consultation rather than
/// cached at startup — it is a cheap property read, and caching it would mean
/// toggling the switch in System Settings did nothing until the app was
/// restarted, which is the class of bug this whole sweep is closing.
///
/// `false` where the platform has no such notion, which is the honest answer:
/// it means "nothing has asked us to reduce motion", not "motion is wanted".
#[cfg(target_os = "macos")]
#[must_use]
pub fn reduce_motion() -> bool {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

#[cfg(windows)]
#[must_use]
pub fn reduce_motion() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };
    let mut animations_on: i32 = 1;
    // SAFETY: a documented read of a BOOL-sized out parameter we own.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            std::ptr::addr_of_mut!(animations_on).cast(),
            0,
        )
    };
    // A failed query means "we do not know", and guessing *reduce* would turn
    // motion off for everyone on a machine where the call is unavailable.
    ok != 0 && animations_on == 0
}

#[cfg(all(not(windows), not(target_os = "macos")))]
#[must_use]
pub fn reduce_motion() -> bool {
    // GNOME has `org.gnome.desktop.interface enable-animations` and KDE has its
    // own, but reading either means a settings-daemon dependency or shelling
    // out per query. Until one is worth it, nothing has asked us to reduce
    // motion — and `motion.enabled` is still there to turn it off by hand.
    false
}

#[cfg(target_os = "macos")]
pub fn native_control_inset(window: &winit::window::Window) -> Option<(f64, f64)> {
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
pub fn native_control_inset(_window: &winit::window::Window) -> Option<(f64, f64)> {
    None
}

/// Hand a file to the OS's default handler — "Edit as TOML" (design §11).
///
/// One call, fire-and-forget: the settings tab must not block on an editor,
/// and a handler that fails does so in the OS's own UI. `cmd /c start` on
/// Windows (`start` is a cmd built-in, not a program), `open` on macOS,
/// `xdg-open` elsewhere.
/// Hand a URL to the default browser — the sign-in hand-off's approval page
/// (#226). [`open_path`]'s per-OS table with a string argument: a URL is not
/// a filesystem path, and shoving one through a `Path` invites separator
/// rewriting on exactly the platform (`cmd /c start`) where it matters.
pub fn open_url(url: &str) {
    if !url_is_shell_safe(url) {
        tracing::warn!(url, "refusing to open a URL a shell could reparse");
        return;
    }
    #[cfg(windows)]
    // The empty quoted argument is start's window title slot — open_path's
    // note; and `start` is the one launcher that hands a URL scheme to the
    // browser rather than the shell.
    let result = std::process::Command::new("cmd").args(["/c", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = result {
        tracing::warn!(error = %e, url, "could not open the browser");
    }
}

/// Whether a URL may be handed to a launcher that re-parses what it is given.
///
/// `cmd /c start` is a shell: `&`, `|`, `<`, `>`, `^` and `%` are
/// metacharacters there, so a URL carrying one is a second command or a
/// silent failure rather than a page. **Refused rather than quoted** — cmd's
/// quoting interacts with `^` and `%` in ways subtle enough that a quoting
/// bug would be invisible until it was an injection, and nothing legitimate
/// here contains them: the URL is this app's control-plane origin plus a
/// base64url grant id, and base64url is `[A-Za-z0-9_-]`. `https` for the same
/// reason `zest_cloud::http::Endpoint` insists on it — the flow's own
/// requests cannot speak anything else, so a page on another scheme is a
/// misconfiguration, not a destination.
///
/// Checked on every platform rather than under `#[cfg(windows)]`, so the rule
/// is exercised wherever the tests run and cannot rot on the one platform
/// that needs it.
fn url_is_shell_safe(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() > "https://".len()
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'`&|<>^%".contains(c))
}

pub fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    // The empty quoted argument is start's window title slot: without it a
    // quoted *path* is parsed as the title and nothing opens.
    let result = std::process::Command::new("cmd")
        .args(["/c", "start", ""])
        .arg(path)
        .spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = result {
        tracing::warn!(error = %e, path = %path.display(), "could not open the file externally");
    }
}

#[cfg(test)]
mod tests {
    use super::url_is_shell_safe;

    #[test]
    fn the_link_pages_own_url_is_openable() {
        assert!(url_is_shell_safe(
            "https://zesterm.sigx.workers.dev/link?grant=Ab_9-xYz01234567890abcdefghijklmnopqrstuv"
        ));
        assert!(
            url_is_shell_safe("https://127.0.0.1:8787/link?grant=abc"),
            "a port and a loopback host are ordinary, and a dev control plane uses both"
        );
    }

    #[test]
    fn a_url_a_shell_could_reparse_is_refused() {
        // `cmd /c start` re-parses its argument, so each of these is a second
        // command or a silent non-open rather than a page. Quoting is not the
        // mechanism here — refusal is — because cmd's rules around `^` and
        // `%` are subtle enough that a quoting bug hides until it is an
        // injection.
        for bad in [
            "https://host/link?grant=a&calc",
            "https://host/link?grant=a|whoami",
            "https://host/link?grant=a>out.txt",
            "https://host/link?grant=a<in.txt",
            "https://host/link?grant=a^b",
            "https://host/link?grant=%PATH%",
            "https://host/link?grant=\"a\"",
            "https://host/link?grant=a b",
            "http://host/link?grant=a",
            "https://",
            "",
        ] {
            assert!(!url_is_shell_safe(bad), "{bad:?} must not reach a launcher");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_backdrop_maps_to_exactly_one_macos_outcome() {
        use objc2_app_kit::NSVisualEffectMaterial;
        use zest_config::settings::Backdrop;

        // The half that can be asserted without a compositor. The rest of
        // `set_backdrop` is AppKit calls whose effect is a look on a screen,
        // and claiming a test covers that would be worse than admitting it
        // does not.
        assert_eq!(
            super::material_for(Backdrop::Vibrancy),
            Some(NSVisualEffectMaterial::UnderWindowBackground)
        );

        // `None` maps to "no material", and the caller reads that as *remove
        // the effect view* rather than "leave whatever is there". A live
        // change from vibrancy to none that only stopped adding one would
        // leave the blur behind for ever, which is the bug this pairing
        // exists to prevent.
        assert_eq!(super::material_for(Backdrop::None), None);

        // Windows materials on a Mac: no backdrop, not a guess at what Mica
        // would have looked like. A config shared between two machines names
        // `mica` on both, and inventing a local equivalent would make the two
        // windows disagree about what the setting means.
        for windows_only in [Backdrop::Mica, Backdrop::MicaAlt, Backdrop::Acrylic] {
            assert_eq!(
                super::material_for(windows_only),
                None,
                "{windows_only:?} is a Windows material; macOS has none of it"
            );
        }
    }
}
