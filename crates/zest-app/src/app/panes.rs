//! Split panes: making one, sizing them, and where each is drawn.
//!
//! Moved out of `app/mod.rs` (#554). The pane vector lives on `Tab`, not here
//! and not on `App` -- panes belong to the tab that owns them, which is what
//! lets a tab move to another window whole (ADR-018).
//!
//! `pane_is_covered` is the rule a screen and a split have to agree on: a split
//! under a full-pane screen must stop drawing, or the screen renders over a
//! grid that is still painting under it. It is a free function with its own
//! test for that reason -- the condition is cheap to state and expensive to get
//! wrong in the middle of a redraw.

use super::*;

/// Whether a full-pane screen owns the grid area this frame — the fleet
/// directory, the theme gallery, or one of the two app tabs.
///
/// **Nothing of the terminal may be drawn when this is true**, and "covered by
/// an opaque panel" is not the same thing. A screen's ground is one SDF rect,
/// and an SDF rect's boundary pixels are antialiased: along its own outermost
/// row and column it is roughly 85% opaque, not 100%. Whatever sits underneath
/// therefore bleeds through a one-pixel frame. The symptom was a stray
/// accent-coloured bracket at the pane's top-left corner (#253) — the block
/// cursor, at the grid origin, showing through the screen's own edge, coming
/// and going with the cursor blink and so reading as a flake rather than as
/// geometry.
///
/// Skipping the grid entirely is also the cheaper answer: the terminal's cell
/// backgrounds and every glyph on it were being shaped, atlased and uploaded
/// each frame to be painted over.
pub(super) fn pane_is_covered(screen: AppScreen, app_tab_active: bool) -> bool {
    screen != AppScreen::Terminal || app_tab_active
}

impl App {
    /// ⌘D: give the active tab one more pane, on the window's own host. Any
    /// number of times — two panes was design screen 5's picture, not a cap
    /// (#436). Moving the keyboard between panes is ⌘U / ⌘J, and a pane on
    /// a *different* host is ⌘H, which routes through the fleet picker.
    pub(super) fn split_right(&mut self) {
        if self.tabs.active().is_none() {
            return;
        }

        // Sized for the pane it will occupy, not the whole grid: the shell's
        // first prompt should wrap where the pane edge is.
        let (cols, rows) = self.split_pane_dims();

        // Seeded with the tab's palette, not the window's: the pane shares
        // its tab's identity until panes carry their own profile.
        let seed = self.palette_for(self.tabs.active().and_then(|t| t.identity.as_ref()));

        let pane = match (&self.route, &self.client_identity) {
            (Some(route), Some(identity)) => {
                let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
                    self.shared.mint_placeholder(),
                )));
                let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
                let command = match route {
                    HostRoute::LocalSocket(_) => self.config.shell.clone().unwrap_or_default(),
                    // Remote either way: the far host runs its own default.
                    HostRoute::Tcp(_) | HostRoute::Relay { .. } => String::new(),
                };
                // A pane shares its tab's identity until panes carry their own
                // profile, and an identity that is a colour but not an
                // environment is only half of one: splitting a tab running one
                // account's CLI would hand the new pane a different account.
                //
                // The tab's own launch environment, **verbatim**: not the
                // profile's half re-combined with whatever `shell.env` says
                // now, which would give a pane a different environment from
                // the tab it split the moment that setting changed. The tab
                // carries this for exactly this use (`Tab::launch_env`), and
                // re-deriving it here would be the second copy that drifts.
                //
                // A tab with no launch environment (an ordinary ⌘T shell)
                // yields an empty vector, and the host applies its own
                // `shell.env` as it does for any launch — so the plain case
                // is unchanged.
                let (tab_env, tab_profile) = self
                    .tabs
                    .active()
                    .map(|t| {
                        let name =
                            t.identity.as_ref().map(|i| i.name.clone()).unwrap_or_default();
                        (t.launch_env.clone(), name)
                    })
                    .unwrap_or_default();
                let env: Vec<(String, String)> = tab_env;
                let session = crate::remote::RemoteSession::create_and_attach(
                    route.dialer(),
                    &crate::remote::AttachOptions {
                        identity,
                        label: "zesterm",
                        command: &command,
                        cwd: "",
                        env: &env,
                        profile: &tab_profile,
                        cols,
                        rows,
                        scrollback: self.config.scrollback,
                        adopt: false,
                        local: route.is_local(),
                        expect_host: None,
                        // Inline on the event loop over the window's already
                        // proven route: a pend here could not paint anyway.
                        on_pending: None,
                    },
                    wake,
                );
                match session {
                    Ok(session) => {
                        *cell.lock() = session.addr();
                        self.seed_terminal(&mut session.terminal().lock(), seed);
                        let local = route.is_local();
                        crate::tabs::SplitPane::daemon(session, local, (cols, rows))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not open a split pane");
                        return;
                    }
                }
            }
            _ => {
                let addr = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                match Session::spawn(
                    &self.build_spec(None).0,
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&self.proxy, cell, Arc::clone(&self.activity)),
                ) {
                    Ok(session) => {
                        self.seed_terminal(&mut session.terminal().lock(), seed);
                        crate::tabs::SplitPane::in_process(session, addr, (cols, rows))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not spawn a split pane");
                        return;
                    }
                }
            }
        };

        self.adopt_pane(pane);
    }

    /// Append a pane to the active tab and give it the keyboard; every pane
    /// is then re-fitted, because one more column narrows all the others.
    pub(super) fn adopt_pane(&mut self, pane: crate::tabs::SplitPane) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.panes.push(pane);
            tab.focus = tab.pane_count() - 1;
        }
        self.resize_split_panes();
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Cols/rows the *next* pane of the active tab will get, from the
    /// current window: the tab's panes plus one, equal columns.
    fn split_pane_dims(&self) -> (u16, u16) {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return (80, 24) };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let n = self.tabs.active().map_or(1, Tab::pane_count) + 1;
        let frames = crate::chrome::layout::pane_frames(area, scale, n);
        let body = crate::chrome::layout::pane_body(frames[n - 1], scale, self.config.padding);
        let cm = fonts.cell_metrics();
        let cols = ((body[2] / cm.cell_w as f32) as u16).max(2);
        let rows = ((body[3] / cm.cell_h as f32) as u16).max(2);
        (cols, rows)
    }

    /// ⌘H's landing, and the picker's when it carries a split: a pane on
    /// `route`, either a fresh shell (`attach: None`) or an existing session
    /// attached as a pane (#436). The pane goes up NOW under a placeholder,
    /// in the connecting treatment a profile launch gets (#175), and a
    /// worker dials — a cold host must cost a placeholder, never a frozen
    /// event loop.
    pub(super) fn spawn_pane_worker(
        &mut self,
        route: HostRoute,
        attach: Option<zest_proto::SessionAddr>,
        expect_host: Option<zest_proto::HostId>,
        host_label: String,
    ) {
        if self.tabs.active().is_none() {
            return;
        }
        let local = route.is_local();
        let identity = if local { self.client_identity.clone() } else { self.remote_identity() };
        let Some(identity) = identity else {
            tracing::warn!("no identity to dial with; cannot open the pane");
            return;
        };

        let (cols, rows) = self.split_pane_dims();
        let seed = self.palette_for(self.tabs.active().and_then(|t| t.identity.as_ref()));
        let provenance = match attach {
            Some(addr) => format!("Attaching \u{b7} {addr} on {host_label}"),
            None => format!("New pane \u{b7} shell on {host_label}"),
        };
        let pending = crate::tabs::PendingSession::new(
            cols,
            rows,
            seed.clone(),
            &host_label,
            &provenance,
            &host_label,
        );
        let placeholder = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
        self.adopt_pane(crate::tabs::SplitPane::connecting(placeholder, pending, (cols, rows)));

        let cell = Arc::new(parking_lot::Mutex::new(placeholder));
        let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
        let proxy = self.proxy.clone();
        let outcomes = Arc::clone(&self.pending_launches);
        let scrollback = self.config.scrollback;
        let command =
            if local { self.config.shell.clone().unwrap_or_default() } else { String::new() };
        // Owned before the worker takes it, beside `command`, and empty for a
        // remote host for `command`'s own reason: `shell.env` is a machine's
        // setting and the far daemon applies its own (#488).
        let env = if local { self.config.shell_env.clone() } else { Vec::new() };
        let on_pending = (!local).then(|| self.pairing_notifier(host_label.clone()));
        let pairing = Arc::clone(&self.pairing);
        let spawned = std::thread::Builder::new().name("zest-pane-open".into()).spawn(move || {
            let opts = crate::remote::AttachOptions {
                identity: &identity,
                label: "zesterm",
                command: &command,
                cwd: "",
                env: &env,
                profile: "",
                cols,
                rows,
                scrollback,
                adopt: false,
                local,
                expect_host,
                on_pending,
            };
            let result = match attach {
                Some(addr) => {
                    crate::remote::RemoteSession::attach_existing(route.dialer(), addr, &opts, wake)
                }
                None => crate::remote::RemoteSession::create_and_attach(route.dialer(), &opts, wake),
            };
            clear_pairing(&pairing, &proxy);
            let outcome = result.map_err(|e| e.to_string()).inspect(|session| {
                *cell.lock() = session.addr();
                session.terminal().lock().set_palette(seed);
            });
            outcomes.lock().push((placeholder, outcome));
            let _ = proxy.send_event(Wakeup::TabsChanged);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the pane worker");
            if let Some(tab) = self.tabs.find_pane_owner(placeholder) {
                if let Some(pane) = tab.panes.iter_mut().find(|p| p.addr == placeholder) {
                    pane.resolve_failed("no thread for the pane worker");
                }
            }
        }
    }

    /// The rectangle each pane's terminal is drawn in, left to right: one
    /// element for an unsplit tab, `pane_count()` for a split one.
    ///
    /// The block headers ride these rectangles, and so does every pixel↔cell
    /// mapping through [`Self::focused_view_rect`] — which is an index into
    /// this list rather than a second copy of the arithmetic, so a header
    /// cannot land one letterbox-offset away from the glyph it sits on.
    pub(super) fn pane_view_rects(&self) -> Vec<[f32; 4]> {
        let Some(window) = self.window.as_ref() else { return Vec::new() };
        let Some(fonts) = self.fonts.as_ref() else { return Vec::new() };
        let Some(tab) = self.tabs.active() else { return Vec::new() };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        // The grid, not the pane, decides the final rectangle: under size
        // arbitration (#215) the session is the smallest attached client's
        // size, and a grid smaller than this pane sits centered in it. Reading
        // the granted size from the terminal -- not from `tab.sized`, which
        // records what this window *asked* -- is what keeps the letterbox
        // aligned with the pixels the renderer actually draws.
        // Index-parallel with the panes, including the ones holding a file
        // (#464): a file pane still occupies a frame, so dropping it here
        // would shift every later pane's rectangle onto its neighbour. Its
        // own entry is a placeholder — the letterbox it produces is never
        // read, because a file pane is drawn by the chrome and gets no
        // viewport — and one cell rather than zero keeps the arithmetic in
        // `pane_grid_rects` away from a division by nothing.
        let grids: Vec<(usize, usize)> = (0..tab.pane_count())
            .map(|i| {
                tab.pane_session(i).map_or((1, 1), |s| {
                    let term = s.terminal().lock();
                    (term.grid().cols(), term.grid().rows())
                })
            })
            .collect();
        crate::chrome::layout::pane_grid_rects(
            area,
            scale,
            self.config.padding,
            &grids,
            fonts.cell_metrics(),
        )
    }

    /// The rectangle the focused terminal is drawn in: the grid area, or the
    /// focused pane's body when the active tab is split. Everything that
    /// maps pixels to cells reads this — one rectangle, one truth.
    pub(super) fn focused_view_rect(&self) -> Option<[f32; 4]> {
        let tab = self.tabs.active()?;
        let rects = self.pane_view_rects();
        rects.get(tab.focus.min(rects.len().saturating_sub(1))).copied()
    }

    /// Resize every pane of the active split tab to its body rectangle —
    /// only the ones whose size actually changed, so a settle or a focus
    /// change costs no resize round-trips.
    pub(super) fn resize_split_panes(&mut self) {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let cm = fonts.cell_metrics();
        let dims = |body: [f32; 4]| {
            (
                ((body[2] / cm.cell_w as f32) as u16).max(2),
                ((body[3] / cm.cell_h as f32) as u16).max(2),
            )
        };
        // Copied out before the mutable borrow of the tab below.
        let padding = self.config.padding;
        let Some(tab) = self.tabs.active_mut() else { return };
        if !tab.is_split() {
            return;
        }
        let frames = crate::chrome::layout::pane_frames(area, scale, tab.pane_count());
        let fit: Vec<(u16, u16)> = frames
            .iter()
            .map(|f| dims(crate::chrome::layout::pane_body(*f, scale, padding)))
            .collect();
        if tab.sized != fit[0] {
            tab.source().resize(fit[0].0, fit[0].1);
            tab.sized = fit[0];
        }
        for (pane, d) in tab.panes.iter_mut().zip(&fit[1..]) {
            // A file pane has no pty to tell, and no cell size to be told in:
            // its scroll clamp is lines and pixels, and the layout pass reads
            // its body rectangle directly.
            let Some(session) = pane.session() else { continue };
            if pane.sized != *d {
                session.resize(d.0, d.1);
                pane.sized = *d;
            }
        }
    }

    pub(super) fn build_panes_model(
        &self,
        fleet_hosts: &[crate::fleet::FleetHost],
    ) -> Option<Vec<crate::chrome::model::PaneModel>> {
        use crate::chrome::model::PaneModel;
        let tab = self.tabs.active()?;
        if !tab.is_split() {
            return None;
        }
        // How many lines a pane body can show, worked out once: slicing where
        // the geometry is known keeps the layout pass pure and stops it having
        // to skip past a hundred thousand lines it will not draw.
        let rows = self.editor_body_rows(tab.pane_count());
        let describe = |source: &dyn crate::source::SessionSource| {
            let (host, accent, remote) = match source.origin() {
                Origin::Daemon { host, local: false } => (host, 1, true),
                _ => (
                    fleet_hosts
                        .iter()
                        .find(|h| h.local)
                        .map_or_else(|| "local".to_string(), |h| h.label.clone()),
                    0,
                    false,
                ),
            };
            let cwd = {
                let term = source.terminal();
                let term = term.lock();
                if term.cwd().is_empty() {
                    term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                } else {
                    term.cwd().to_string()
                }
            };
            let cwd = if remote { cwd } else { crate::status::shorten_home(&cwd) };
            let path = fleet_hosts
                .iter()
                .find(|h| h.label == host)
                .and_then(|h| match h.reachability {
                    Some(zest_mesh::Reachability::Cloud) => Some(match h.rtt_ms {
                        Some(ms) => {
                            format!("tunnel {}", crate::chrome::layout::format_ms(ms))
                        }
                        None => "tunnel".to_string(),
                    }),
                    _ => None,
                });
            let sub = match (cwd.is_empty(), path) {
                (false, Some(p)) => format!("{cwd} · {p}"),
                (false, None) => cwd,
                (true, Some(p)) => p,
                (true, None) => String::new(),
            };
            (host, sub, accent)
        };
        Some(
            (0..tab.pane_count())
                .map(|i| {
                    // A file pane names the file, not a host: the header's job
                    // is to say which pane you are looking at, and "local" on
                    // a pane showing `main.rs` says nothing (#464).
                    let (host, sub, accent, kind) = match tab.pane_session(i) {
                        Some(session) => {
                            let (host, sub, accent) = describe(session);
                            (host, sub, accent, PaneKind::Session)
                        }
                        None => {
                            let e = tab.pane_editor(i).expect("a pane is a session or a file");
                            (
                                e.title().to_string(),
                                crate::status::shorten_home(e.dir()),
                                0,
                                PaneKind::Editor(self.editor_view(e, rows)),
                            )
                        }
                    };
                    PaneModel { host, sub, focused: i == tab.focus, accent, kind }
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod pane_cover_tests {
    use super::{pane_is_covered, AppScreen};

    #[test]
    fn every_full_pane_screen_takes_the_terminal_off_the_frame() {
        // The rule this table encodes is "not drawn", not "drawn and hidden":
        // a screen's ground is one SDF rect, and the outermost row and column
        // of an SDF rect are antialiased to roughly 85%, so the terminal
        // underneath bleeds through a one-pixel frame however opaque the fill
        // is. #253 was the block cursor doing exactly that at the pane's
        // top-left corner.
        for screen in [AppScreen::Fleet, AppScreen::Themes] {
            assert!(pane_is_covered(screen, false), "{screen:?} owns the whole pane");
        }
        assert!(
            pane_is_covered(AppScreen::Terminal, true),
            "an app tab covers the pane without being an AppScreen of its own — \
             which is now true of Profiles too, and was the whole of #494: it \
             had a variant here, so it could not also be a tab"
        );
        assert!(
            !pane_is_covered(AppScreen::Terminal, false),
            "the terminal is the terminal — this is the everyday frame and it must build"
        );
    }

    #[test]
    fn an_overlay_is_not_a_cover() {
        // The palette, the launcher and the fleet picker float *over* the
        // terminal and it has to keep rendering underneath them. They are not
        // in this predicate at all, and the check that they stay out of it is
        // that `AppScreen` is still `Terminal` while they are open.
        assert!(!pane_is_covered(AppScreen::Terminal, false));
    }
}
