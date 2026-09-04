//! Platform-specific window setup.

/// What this window calls itself to the desktop: Wayland `app_id`, X11
/// `WM_CLASS`.
///
/// Nothing set either before #472, which costs more than a name: on Wayland the
/// taskbar icon is resolved by matching `app_id` against an installed
/// `.desktop` file, so without it there is no icon at all and
/// `with_window_icon` cannot rescue it (winit ignores that on Wayland). Window
/// rules -- Hyprland's `windowrule`, KWin's -- have nothing to match on either.
///
/// One lowercase spelling for both, matching the binary and the window title.
/// `packaging/linux/zesterm.desktop` is the other end of it: its basename, its
/// `Icon=` and its `StartupWMClass=` must all be this string, or the icon
/// lookup finds nothing. `the_app_id_and_the_desktop_entry_agree` reads the
/// entry at compile time so a rename on one side is a build failure rather
/// than a missing icon.
/// Deliberately *not* X11's capitalized
/// convention (`("zesterm", "Zesterm")`): Hyprland matches `class:` against the
/// app_id for a Wayland window and against WM_CLASS's *class* for an XWayland
/// one, so two spellings is a rule that works in one session and silently does
/// nothing in the other.
///
/// Unix-only, because the concept is: Windows and macOS identify a window by
/// its executable and bundle, so there is no string to carry and an
/// unconditional constant would be dead code on two of the three legs.
#[cfg(all(unix, not(target_os = "macos")))]
pub const APP_ID: &str = "zesterm";

#[cfg(all(unix, not(target_os = "macos")))]
#[cfg(test)]
mod identity_tests {
    /// The desktop entry and the window must agree, or the window has no icon.
    ///
    /// winit exposes **no getter** for `app_id` or `WM_CLASS`, so this contract
    /// cannot be asserted against a live window at all — the packaged file is
    /// the only other end of it. Reading the entry at compile time is what
    /// makes "rename one, forget the other" a build failure instead of a
    /// missing icon nobody traces back here.
    #[test]
    fn the_app_id_and_the_desktop_entry_agree() {
        const ENTRY: &str = include_str!("../../../packaging/linux/zesterm.desktop");

        let wm_class = ENTRY
            .lines()
            .find_map(|l| l.strip_prefix("StartupWMClass="))
            .expect("the entry declares StartupWMClass");
        assert_eq!(
            wm_class,
            super::APP_ID,
            "StartupWMClass must equal APP_ID, or a Wayland compositor matches the \
             window against no desktop file and shows no icon"
        );

        let icon = ENTRY
            .lines()
            .find_map(|l| l.strip_prefix("Icon="))
            .expect("the entry declares an Icon");
        assert_eq!(
            icon, super::APP_ID,
            "the icon is looked up by this name under hicolor; PKGBUILD installs \
             it as APP_ID.svg"
        );

        // The basename matters as much as the contents: the lookup is
        // `app_id` -> `<app_id>.desktop`, so a renamed file breaks it
        // silently.
        //
        // Derived from `APP_ID` and read off disk, not compared against a
        // literal: a literal only restates the `include_str!` path above, so
        // renaming the file and that path together would keep this green
        // while a compositor still looked for `zesterm.desktop` and found
        // nothing. Comparing the bytes closes the last gap — the entry this
        // test read and the entry packaged under `APP_ID`'s name are then
        // provably the same file.
        let packaged = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/linux")
            .join(format!("{}.desktop", super::APP_ID));
        let on_disk = std::fs::read_to_string(&packaged).unwrap_or_else(|e| {
            panic!(
                "the compositor looks up `<app_id>.desktop`, so an entry has to be packaged \
                 under that name: {} ({e})",
                packaged.display()
            )
        });
        assert_eq!(
            on_disk, ENTRY,
            "the entry asserted above must be the one named for APP_ID, or this test is \
             checking a file nothing installs"
        );
    }
}

/// Stamp [`APP_ID`] onto the window attributes.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn identify(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland;
    use winit::platform::x11::WindowAttributesExtX11;
    // Fully-qualified, not method syntax: both extension traits declare
    // `with_name` with the same signature and different meanings for the first
    // argument, so `attrs.with_name(..)` is ambiguous and will not compile.
    // Both are applied because winit compiles both backends; the one that is
    // not running never reads its field.
    let attrs = WindowAttributesExtWayland::with_name(attrs, APP_ID, APP_ID);
    WindowAttributesExtX11::with_name(attrs, APP_ID, APP_ID)
}

/// Windows and macOS identify a window by its executable and bundle, not by a
/// string on the surface, so there is nothing to stamp.
#[cfg(any(windows, target_os = "macos"))]
pub fn identify(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    attrs
}

/// Which display server this process actually connected to.
///
/// Read from the display handle, never from `XDG_SESSION_TYPE` or
/// `WAYLAND_DISPLAY`: an XWayland run has both set and *is* X11, which is
/// precisely the case where the answer changes what `window.opacity` can do.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Session {
    /// Windows, macOS, or a unix display server we do not distinguish.
    #[default]
    Other,
    Wayland,
    X11,
}

impl Session {
    /// Ask the display handle what it is.
    pub fn of(h: &impl raw_window_handle::HasDisplayHandle) -> Self {
        use raw_window_handle::RawDisplayHandle;
        match h.display_handle().map(|d| d.as_raw()) {
            Ok(RawDisplayHandle::Wayland(_)) => Self::Wayland,
            Ok(RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)) => Self::X11,
            _ => Self::Other,
        }
    }
}

/// What this process cannot honour, in the settings form's own terms.
///
/// ADR-003 promised a `Capabilities` value reported to the settings layer, and
/// said the fallback must be visible rather than silent. This is that promise
/// at the size the problem turned out to be: three observed facts, feeding the
/// `inert` flag and the `Notice` row the settings screen already draws. A
/// variant-per-capability enum would have identical call sites and one more
/// table to keep in step with the schema.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// The surface actually composites per-pixel alpha. **Observed** from the
    /// configured `alpha_mode`, not guessed from the platform: on Windows this
    /// is adapter-dependent (ADR-003) and on X11 it depends on the visual the
    /// window was built with.
    pub transparency: bool,
    /// `window.opacity` takes effect without a relaunch. False on X11, where
    /// winit's `set_transparent` is an empty function and the ARGB visual is
    /// fixed when the window is created.
    pub live_opacity: bool,
    /// A platform backdrop material exists at all. Compile-time: there is no
    /// Linux implementation to probe, and probing for one would be inventing
    /// it.
    pub backdrop: bool,
}

impl Default for Capabilities {
    /// Everything works. The honest default for a process that has not yet
    /// built a surface — claiming a limit we have not observed would grey out
    /// a control that turns out to be fine.
    fn default() -> Self {
        Self { transparency: true, live_opacity: true, backdrop: true }
    }
}

impl Capabilities {
    /// Whether this platform can deliver what the key promises.
    ///
    /// Only keys that are *wired and undeliverable* answer `false`; a key this
    /// build simply does not read is `NOT_YET_WIRED`'s business, which is a
    /// different fact and a different list.
    #[must_use]
    pub fn honours(&self, key: &str) -> bool {
        match key {
            "window.backdrop" => self.backdrop,
            // Both opacities, because they are one capability: each is an
            // alpha this surface either composites or does not
            // (`Config::translucent_surface` reads them together for exactly
            // that reason). Listing only one would leave the chrome's slider
            // live on a surface that cannot honour it.
            "window.opacity" | "window.chrome_opacity" => self.transparency,
            _ => true,
        }
    }

    /// The Window category's banner, or `None` when everything it offers works.
    ///
    /// Says what *does* work rather than only what does not: on the platform
    /// where the backdrop is missing, the compositor is usually the thing that
    /// blurs, and a notice that only refuses leaves the user with nowhere to
    /// go.
    #[must_use]
    pub fn window_notice(&self) -> Option<String> {
        let mut out: Vec<&str> = Vec::new();
        if !self.backdrop {
            out.push(
                "`window.backdrop` has no effect here: no Wayland protocol offers a blur \
                 material and X11's are compositor-specific. Hyprland and KWin blur behind \
                 translucent windows themselves -- set `window.opacity` below 1 and turn \
                 blur on in the compositor.",
            );
        }
        if !self.transparency {
            out.push(
                "`window.opacity` and `window.chrome_opacity` are ignored: this surface \
                 cannot composite per-pixel alpha, so the window is opaque whatever they \
                 say.",
            );
        } else if !self.live_opacity {
            out.push(
                "`window.opacity` and `window.chrome_opacity` apply at the next launch: the \
                 visual carrying the alpha is chosen when the window is created, and X11 \
                 cannot swap it afterwards.",
            );
        }
        (!out.is_empty()).then(|| out.join(" "))
    }
}

#[cfg(test)]
mod capability_tests {
    use super::Capabilities;

    /// A banner that is always up is wallpaper; one that never appears is the
    /// warn-only status quo this replaces.
    #[test]
    fn a_notice_appears_exactly_when_something_is_ignored() {
        assert_eq!(Capabilities::default().window_notice(), None, "nothing to say when all of it works");
        let no_blur = Capabilities { backdrop: false, ..Capabilities::default() };
        assert!(no_blur.window_notice().is_some_and(|t| t.contains("window.backdrop")));
        let opaque = Capabilities { transparency: false, ..Capabilities::default() };
        assert!(opaque.window_notice().is_some_and(|t| t.contains("window.opacity")));
    }

    /// Both opacity keys are one capability, and the banner names both.
    ///
    /// `window.chrome_opacity` arrived after this sweep was written (#522) and
    /// is an alpha onto the same surface: a build that refused one and left
    /// the other live would grey out the window slider while the chrome
    /// slider sat there doing nothing.
    #[test]
    fn both_opacities_stand_or_fall_together() {
        let opaque = Capabilities { transparency: false, ..Capabilities::default() };
        assert!(!opaque.honours("window.opacity"));
        assert!(!opaque.honours("window.chrome_opacity"));
        let notice = opaque.window_notice().expect("a surface with no alpha has something to say");
        assert!(notice.contains("window.chrome_opacity"), "the banner names both: {notice}");

        let live = Capabilities::default();
        assert!(live.honours("window.opacity") && live.honours("window.chrome_opacity"));

        let x11 = Capabilities { live_opacity: false, ..Capabilities::default() };
        assert!(
            x11.window_notice().is_some_and(|t| t.contains("window.chrome_opacity")),
            "and so does the relaunch sentence"
        );
    }

    /// The two opacity sentences are mutually exclusive: a surface that cannot
    /// composite alpha at all must not also be told it will work next launch.
    #[test]
    fn an_opaque_surface_is_not_promised_a_relaunch() {
        let c = Capabilities { transparency: false, live_opacity: false, backdrop: true };
        let t = c.window_notice().expect("something is ignored");
        // "ignored", not the whole clause: the sentence names both opacity
        // keys and so reads plural, and this test is about which of the two
        // sentences appears rather than how either is worded.
        assert!(t.contains("ignored"), "{t}");
        assert!(!t.contains("next launch"), "an opaque surface gains nothing by relaunching: {t}");
    }

    /// `inert` must follow the capability, and must not spread to keys the
    /// platform can perfectly well deliver.
    #[test]
    fn only_the_undeliverable_keys_are_refused() {
        let c = Capabilities { transparency: false, live_opacity: false, backdrop: false };
        assert!(!c.honours("window.backdrop"));
        assert!(!c.honours("window.opacity"));
        assert!(c.honours("window.padding"), "an unrelated key must stay live");
        assert!(c.honours("appearance.theme"));
    }

    /// Every dotted key the notice names has to be a real setting, or the
    /// banner is nonsense pointing at nothing.
    #[test]
    fn the_notice_names_real_settings() {
        let c = Capabilities { transparency: false, live_opacity: false, backdrop: false };
        let text = c.window_notice().expect("something is ignored");
        let keys = zest_config::schema::keys();
        for word in text.split('`').skip(1).step_by(2) {
            if word.contains('.') && !word.contains(' ') {
                assert!(keys.iter().any(|k| k == word), "notice names `{word}`, which is not a setting");
            }
        }
    }
}

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

/// Hand a URL to the default browser — the sign-in hand-off's approval page
/// (#226). [`open_path`]'s per-OS table with a string argument: a URL is not
/// a filesystem path, and shoving one through a `Path` invites separator
/// rewriting on exactly the platform where it matters.
pub fn open_url(url: &str) {
    if !url_is_shell_safe(url) {
        tracing::warn!(url, "refusing to open a URL a shell could reparse");
        return;
    }
    #[cfg(windows)]
    shell_open(std::ffi::OsStr::new(url), "browser");
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        let result = zest_daemon::spawn::quiet_command("open").arg(url).spawn();
        #[cfg(not(target_os = "macos"))]
        let result = zest_daemon::spawn::quiet_command("xdg-open").arg(url).spawn();
        if let Err(e) = result {
            tracing::warn!(error = %e, url, "could not open the browser");
        }
    }
}

/// Hand something to the OS's default handler, off this thread.
///
/// `ShellExecuteW` rather than the `cmd /c start` this used to be, for three
/// reasons that all point the same way. It is the call `cmd` makes internally,
/// so the shell was a whole process of overhead on the way to it. It takes the
/// target as an *argument* rather than as text something re-parses, which is
/// #403's constructive half and the reason [`url_is_shell_safe`] is now a
/// second fence rather than the only one. And `cmd.exe` is a console program:
/// launched from a GUI-subsystem binary that owns no console — which is what
/// zesterm is when Explorer starts it — Windows mints a console for it and
/// flashes a window on screen (#461).
///
/// On a thread of its own because it blocks while the OS resolves the handler,
/// and a cold browser start is not fast; a click that opens a link must not
/// stall the frame. COM is initialized there because the handlers this reaches
/// are COM objects, apartment-threaded as a UI launch expects — and this thread
/// exists to be that apartment, which is why it is not the winit one.
#[cfg(windows)]
fn shell_open(target: &std::ffi::OsStr, what: &'static str) {
    use std::os::windows::ffi::OsStrExt as _;

    let wide: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
    let described = target.to_string_lossy().into_owned();
    let spawned = std::thread::Builder::new().name("zesterm-shell-open".into()).spawn(move || {
        use windows_sys::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
        };
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        // "open" as UTF-16, NUL-terminated. The default verb would do for a URL
        // and is not the same thing for a file: a `.ps1` whose default verb is
        // "edit" must still open in an editor, and naming the verb says so.
        const OPEN: [u16; 5] = [b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];

        // The cast is windows-sys 0.60's own inconsistency, not a conversion:
        // `COINIT_APARTMENTTHREADED` is typed `COINIT` (an i32) while the
        // generated `CoInitializeEx` takes a `u32`. Both are the same two bits
        // at the ABI, so `as` is exact -- `try_into().unwrap()` would only add
        // a panic to a value that is a constant.
        // SAFETY: a fresh thread that has not initialized COM.
        let com = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        // SAFETY: both strings are NUL-terminated and live across the call; the
        // remaining pointers are documented as optional.
        let rc = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                OPEN.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // Documented as "a value greater than 32 on success" -- the return is a
        // legacy HINSTANCE and the small values are error codes wearing one.
        if rc as isize <= 32 {
            tracing::warn!(code = rc as isize, target = %described, "could not open the {what}");
        }
        // Every *success* is balanced, not just `S_OK`. `S_FALSE` means this
        // thread already had an apartment -- it is still a success, and it
        // still took a reference, so skipping the release there leaks one.
        // (It cannot happen on a thread we just created and initialized first
        // thing; balancing on the contract rather than on that reasoning is
        // what keeps the next edit correct.) A *failure* -- notably
        // `RPC_E_CHANGED_MODE` -- took no reference and must not be released.
        if com >= 0 {
            // SAFETY: paired with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread to open the {what} on");
    }
}

/// Whether a URL may be handed to a launcher that re-parses what it is given.
///
/// **Now a second fence rather than the only one** (#461): [`shell_open`] hands
/// `ShellExecuteW` the URL as an argument, so nothing re-parses it and none of
/// the characters below are metacharacters any more. It is kept because a
/// launcher is a place worth being narrow at, and because the scheme check is
/// the half that was never about `cmd`. What follows is why it was written.
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

/// Hand a file to the OS's default handler — "Edit as TOML" (design §11).
///
/// One call, fire-and-forget: the settings tab must not block on an editor,
/// and a handler that fails does so in the OS's own UI. [`shell_open`] on
/// Windows, `open` on macOS, `xdg-open` elsewhere. (This paragraph sat above
/// [`open_url`] until #461 — two doc comments had run together, so the item it
/// describes had none.)
pub fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    shell_open(path.as_os_str(), "file");
    #[cfg(not(windows))]
    {
        // Through `quiet_command` even here, where there is no console to
        // suppress: the rule `cargo xtask check-spawn` enforces is that shipped
        // code never calls `Command::new` itself, and an exception carved out
        // for a file whose Windows arm happens not to need one today is an
        // exception the next Windows arm inherits.
        #[cfg(target_os = "macos")]
        let result = zest_daemon::spawn::quiet_command("open").arg(path).spawn();
        #[cfg(not(target_os = "macos"))]
        let result = zest_daemon::spawn::quiet_command("xdg-open").arg(path).spawn();
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %path.display(), "could not open the file externally");
        }
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
