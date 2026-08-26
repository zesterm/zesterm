//! Who draws the window frame.
//!
//! This exists because the fact was a `bool` and every consumer re-decided what
//! it meant. `custom_chrome` was read in three places to switch on our caption
//! buttons and resize bands, but *applied* to the window in one — inside
//! `#[cfg(windows)]`. On Linux that meant `custom_chrome = "on"` drew our
//! caption while the compositor kept its own frame, the two titlebars
//! `ROADMAP.md` warned about for KDE, reachable from the settings UI; on macOS
//! it drew our cluster on the right while the traffic lights stayed on the
//! left. Same gap, two platforms, and a comment on either would not have
//! closed it.
//!
//! So the intent is a type whose accessors read one variant, and whose bad
//! combination — the compositor decorating *and* us drawing a caption — cannot
//! be spelled.

/// The window system a decision is being made for.
///
/// A parameter rather than a `cfg!` so the matrix is testable: the bug is a
/// *combination*, and a table that only ever sees the platform it compiled for
/// cannot show that the other two are safe. One CI leg checks all nine answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Host {
    Windows,
    MacOs,
    /// X11 and Wayland alike: the choice below does not differ between them,
    /// and winit is what negotiates the difference that does.
    Unix,
}

impl Host {
    #[must_use]
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// Who owns the window frame, resolved once from `window.custom_chrome`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowChrome {
    /// The OS or compositor draws the frame. We draw no caption and no resize
    /// bands — `DefWindowProc`, or the compositor, answers for the edge.
    ///
    /// On wlroots this is not a titlebar at all: server-side decoration there
    /// is a border, rounding and a shadow, so there is nothing sitting above
    /// the tab strip to reclaim.
    Server,
    /// We own the frame: created undecorated, with caption buttons and resize
    /// bands out of `chrome::layout`, moves through `Window::drag_window` and
    /// edges through `Window::drag_resize_window` — both of which winit
    /// implements on X11 and Wayland, which is why this is a supported answer
    /// on Linux rather than a refusal.
    Client,
    /// macOS only: the OS frame is kept but made transparent and full-size, so
    /// the traffic lights, native fullscreen and Sequoia tiling all survive and
    /// the tab strip fills the titlebar.
    Integrated,
}

impl WindowChrome {
    /// What `WindowAttributes::with_decorations` is given.
    #[must_use]
    pub fn decorations(self) -> bool {
        !matches!(self, Self::Client)
    }

    /// Whether the chrome layout draws caption buttons and resize bands.
    ///
    /// The other half of [`Self::decorations`], deliberately reading the same
    /// variant: that is the whole point of the type.
    #[must_use]
    pub fn draws_caption(self) -> bool {
        matches!(self, Self::Client)
    }

    /// Resolve the setting against the window system.
    ///
    /// `Auto` is `Server` on unix, and that is forced rather than chosen:
    /// winit 0.30 exposes no getter for the negotiated
    /// `zxdg_toplevel_decoration_v1` mode (`is_decorated()` returns our own
    /// requested flag), so answering `Client` would assert a fact we cannot
    /// check. Deferring to the compositor is the only answer that is right
    /// without knowing which compositor it is — and winit already asks it the
    /// right question.
    ///
    /// macOS resolves `Integrated` for every setting: borderless there costs
    /// the traffic lights, native fullscreen, tiling and accessibility, which
    /// is a decision the project already made. The type enforces it instead of
    /// documenting it.
    #[must_use]
    pub fn resolve(pref: zest_config::settings::CustomChrome, host: Host) -> Self {
        use zest_config::settings::CustomChrome as P;
        match (host, pref) {
            (Host::MacOs, _) => Self::Integrated,
            (Host::Windows, P::Auto | P::On) | (Host::Unix, P::On) => Self::Client,
            (Host::Windows, P::Off) | (Host::Unix, P::Auto | P::Off) => Self::Server,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Host, WindowChrome};
    use zest_config::settings::CustomChrome as P;

    const HOSTS: [Host; 3] = [Host::Windows, Host::MacOs, Host::Unix];
    const PREFS: [P; 3] = [P::Auto, P::On, P::Off];

    /// The bug this type exists to make unspellable: the compositor drawing a
    /// frame *and* us drawing a caption on top of it. Checked over the whole
    /// matrix, on whichever leg runs it, because the combination is what was
    /// wrong and a single-platform check cannot see a combination.
    #[test]
    fn two_titlebars_are_unrepresentable() {
        for host in HOSTS {
            for pref in PREFS {
                let c = WindowChrome::resolve(pref, host);
                assert!(
                    !(c.decorations() && c.draws_caption()),
                    "{host:?} + {pref:?} resolved to {c:?}, which both keeps the \
                     system frame and draws our own caption over it"
                );
            }
        }
    }

    /// The regression test for the reported bug: `on` used to switch our
    /// caption on while leaving `with_decorations(false)` behind a
    /// `#[cfg(windows)]`, so the compositor kept its frame.
    #[test]
    fn on_means_the_compositor_stops_decorating_on_unix() {
        let c = WindowChrome::resolve(P::On, Host::Unix);
        assert_eq!(c, WindowChrome::Client);
        assert!(!c.decorations(), "custom_chrome=on must undecorate the window");
        assert!(c.draws_caption(), "...and then we owe it a caption of our own");
    }

    /// Pins the policy: with no compositor to ask, defer to the one that
    /// negotiated. Changing this should require arguing with `resolve`'s doc.
    #[test]
    fn auto_keeps_system_chrome_on_unix() {
        assert_eq!(WindowChrome::resolve(P::Auto, Host::Unix), WindowChrome::Server);
    }

    /// macOS never goes borderless, for any setting — including `on`, which is
    /// how our caption cluster ended up drawn beside the traffic lights.
    #[test]
    fn macos_never_goes_borderless() {
        for pref in PREFS {
            let c = WindowChrome::resolve(pref, Host::MacOs);
            assert_eq!(c, WindowChrome::Integrated, "{pref:?} must stay integrated on macOS");
            assert!(!c.draws_caption(), "{pref:?} would draw a second caption beside the traffic lights");
        }
    }

    /// Windows keeps the behaviour it shipped with: `auto` is our own chrome.
    #[test]
    fn windows_defaults_to_its_own_chrome() {
        assert_eq!(WindowChrome::resolve(P::Auto, Host::Windows), WindowChrome::Client);
        assert_eq!(WindowChrome::resolve(P::Off, Host::Windows), WindowChrome::Server);
    }
}
