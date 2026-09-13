//! What winit hands the window, and what the window does with it.
//!
//! The second `impl App` block, moved out of `app/mod.rs` whole (#554). These
//! six methods are the surface `Process` calls (ADR-018): it routes each event
//! by window id and each wakeup by `process::route`, and everything below is
//! one window answering for itself.
//!
//! They are inherent methods, so moving the block changes nothing about who can
//! call them -- a `pub(crate) fn` on `App` is reachable wherever `App` is,
//! whichever module the `impl` sits in. `process.rs` needed no edit.
//!
//! `handle_window_event` is 1,886 lines and should not be. Its arms are mostly
//! one-line delegations, and then `KeyboardInput` holds 1,267 lines inline --
//! the overlay chain, each link of which wants to be a function that says
//! whether it handled the key. That is #554's phase 3 and deliberately not this
//! commit: a move and a rewrite in one diff is a diff nobody can review.

use super::*;

impl App {
    /// Create this window's OS window, its fonts, its GPU surface and its
    /// first tab. Everything the old single-window `resumed` did for the
    /// window; what it did for the *process* — the config watcher, the fleet
    /// model — lives in [`crate::process::Process`] now.
    pub(crate) fn open_window(&mut self, el: &ActiveEventLoop, plan: WindowSpec) {
        debug_assert!(self.window.is_none(), "a window opens once");

        let t0 = std::time::Instant::now();

        // Created HIDDEN, shown only once a real frame has been presented.
        //
        // A visible window shows the OS default background -- white on Windows --
        // for as long as startup takes, and startup is several hundred
        // milliseconds: adapter enumeration, device creation, shader
        // compilation, font resolution, then spawning a shell. Painting nothing
        // into a visible window is what produces the white flash; the fix is to
        // not be visible until there is something to show.
        // `window.columns` / `window.rows`, when the user actually set them.
        //
        // Sizing from cells needs cell metrics, which need the font stack, and
        // resolving fonts costs ~30ms here — real work *before the first
        // paint*, which is precisely what `STARTUP_BUDGET_MS` exists to catch.
        // So it is paid only by configs that ask for it: provenance knows
        // whether a layer wrote the key, which `Config` alone cannot, since a
        // value equal to the default is indistinguishable from an absent one.
        //
        // An unset config therefore keeps the historical 960×600 rather than
        // becoming exactly 100×30. Those are not the same — the insets take
        // their share, so the default window is nearer 98×27 — and that gap is
        // pre-existing rather than introduced here, but it is a gap: see #308.
        //
        // A remembered size outranks the setting: a person resized *that*
        // window, and the setting describes windows that have no memory yet.
        let sized_from_cells = plan.geometry.inner_size.is_none()
            && ["window.columns", "window.rows"]
                .iter()
                .any(|k| self.provenance.contains_key(*k));
        let early = (sized_from_cells && self.screenshot.is_none())
            .then(|| self.window_size_in_cells(el))
            .flatten();

        // Asked of the event loop's display handle before the window exists,
        // because `ActiveEventLoop` already knows and the answer decides what
        // `window.opacity` is allowed to promise.
        self.session = crate::platform::Session::of(el);
        let attrs = Window::default_attributes()
            .with_title("zesterm")
            // The cross-platform builder, deliberately: this is the whole of
            // the Linux decoration story, and putting it here is what stops
            // `drawn_caption` and the system frame being decided separately
            // -- which is how the window came to wear two of them (#472).
            .with_decorations(self.config.chrome.decorations())
            .with_transparent(self.config.translucent_surface())
            .with_visible(false);
        let mut attrs = platform::identify(attrs);
        attrs = match (self.screenshot.as_ref(), plan.geometry.inner_size, early.as_ref()) {
            // `--screenshot-size` is an explicit instruction from this
            // invocation and outranks a stored preference.
            (Some(shot), _, _) => attrs
                .with_inner_size(winit::dpi::LogicalSize::new(shot.size.0, shot.size.1)),
            (None, Some([w, h]), _) => attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h)),
            (None, None, Some(sized)) => {
                attrs.with_inner_size(winit::dpi::PhysicalSize::new(sized.width, sized.height))
            }
            (None, None, None) => attrs.with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0)),
        };
        if let Some([x, y]) = plan.geometry.position {
            attrs = attrs.with_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        if plan.geometry.maximized {
            attrs = attrs.with_maximized(true);
        }
        // The launcher's token: without it a Wayland compositor refuses a
        // window opened by a process that was not itself interacted with
        // the right to take focus, and the new window appears behind.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = match plan.activation_token.clone() {
            Some(token) => {
                use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
                attrs.with_activation_token(winit::window::ActivationToken::from_raw(token))
            }
            None => attrs,
        };
        // Not borderless (issue #9, WS-C2: borderless costs traffic lights,
        // native fullscreen, Sequoia tiling and accessibility). A transparent
        // full-size titlebar keeps all of that, and the tab strip is what
        // fills the space — these are attribute flags, so the startup budget
        // pays nothing.
        //
        // Conditioned on the variant rather than on `cfg` alone, so this and
        // `WindowChrome::resolve` cannot drift into disagreeing about what
        // macOS gets.
        #[cfg(target_os = "macos")]
        let attrs = if self.config.chrome == crate::window_chrome::WindowChrome::Integrated {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
        } else {
            attrs
        };
        // Borderless, so the OS caption stops sitting *above* our own tab
        // strip — two titlebars was the state of this window until now.
        //
        // Not a hand-rolled `WM_NCCALCSIZE`, which is what WS-A assumed this
        // would take: winit already returns 0 with the client area covering
        // the frame when decorations are off, and clamps the maximized rect to
        // the monitor work area, which is the thing that keeps the strip on
        // screen. `undecorated_shadow` is what keeps the drop shadow, the snap
        // animation and the rounded corners; it costs one black pixel row
        // along the top, per winit's own comment, and that reads as the
        // window's top border against every theme in the gallery.
        //
        // What it does cost: winit has no `WM_NCHITTEST` handler, so the
        // resize edges vanish with the frame. They come back out of the chrome
        // layout pass as `HitRegion::Resize`.
        #[cfg(windows)]
        let attrs = if self.config.chrome.decorations() {
            attrs
        } else {
            use winit::platform::windows::WindowAttributesExtWindows;
            // All that is left here once `with_decorations` moved to the base
            // builder: the drop shadow, snap animation and rounded corners a
            // borderless window would otherwise lose. Costs one black pixel
            // row along the top, per winit's own comment.
            attrs.with_undecorated_shadow(true)
        };
        let window = Arc::new(el.create_window(attrs).expect("create window"));

        // The OS appearance, before the first paint. `apply_theme` below reads
        // it through `theme_id`, so a light desktop opens light rather than
        // opening dark and correcting itself a frame later. `None` means the
        // platform will not say, which is not the same as "dark" but is the
        // only safe reading of it: every shipped default theme is dark.
        self.system_light = window.theme() == Some(winit::window::Theme::Light);
        if self.config.follow_system_theme {
            self.apply_theme();
        }

        // Show it NOW, painted by the OS in the theme colour.
        //
        // Bringing up the GPU is ~700ms of serial driver init with nothing to
        // overlap it with, so waiting for a presentable frame means the window
        // cannot appear in under three quarters of a second. It does not need
        // the GPU to be the right colour: a class background brush makes Windows
        // erase it on the first paint. The first real frame is the same colour,
        // so the handover is invisible.
        let bg = self.palette.background;
        platform::set_background_color(&window, bg.r, bg.g, bg.b);
        // Before the window is shown, so a backdrop never appears as a second
        // frame after an opaque one. Note the class brush above paints opaque
        // until the first GPU frame lands, so Mica is hidden for that ~700ms
        // and then appears — the same handover the brush exists to make
        // invisible, and not a bug however much it looks like one.
        platform::set_backdrop(&window, self.config.backdrop);
        // Screenshot mode never shows it. The frame goes to a texture we own,
        // so there is nothing to present and no reason to put a window on
        // someone's screen — which is what makes this usable while they are
        // working, and usable at all where there is no screen.
        match self.screenshot.as_ref() {
            Some(shot) => self.screenshot_at = Some(std::time::Instant::now() + shot.delay),
            None => window.set_visible(true),
        }
        let first_paint = t0.elapsed();

        // After the paint, deliberately. Without this no `Ime` event is ever
        // delivered -- and on macOS it is also what makes dead-key sequences
        // combine, so `Option+e` `e` produces `e` rather than `é`. It is off by
        // default in winit because a game does not want it; a terminal always
        // does. Nobody can type before the window exists, so it costs nothing to
        // keep it out of the measured path.
        window.set_ime_allowed(true);
        tracing::debug!(elapsed_ms = first_paint.as_millis(), "window shown");

        // The number the daemon work must not quietly ruin.
        //
        // Attaching to a daemon (ADR-007) puts a find-or-spawn and a socket
        // handshake into startup, and the tempting place to put them is right
        // here — before the window exists, so the session is ready when it does.
        // That would trade a hard-won 50ms for several hundred. This prints on
        // demand so the regression is a failing command rather than a vague
        // sense that it used to feel faster.
        if self.startup_probe {
            println!("first_paint_ms={}", first_paint.as_millis());
            if first_paint > std::time::Duration::from_millis(STARTUP_BUDGET_MS) {
                eprintln!(
                    "FAIL: first paint took {}ms, budget is {STARTUP_BUDGET_MS}ms.\n\
                     Something now runs before the window is shown. The window does \
                     not need a GPU, a font, a shell or a daemon to be the right \
                     colour -- see the comment above this check.",
                    first_paint.as_millis()
                );
                std::process::exit(1);
            }
            el.exit();
            return;
        }

        let scale = window.scale_factor() as f32;
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        // Reuse the stack the sizing pass already built, when it was measured
        // at this window's scale. It usually was — the window opens on the
        // primary monitor — and when it was not, the metrics behind the size
        // are wrong anyway, so rebuilding is the correction rather than a cost.
        let mut fonts = match early {
            Some(sized) if (scale - sized.scale).abs() < f32::EPSILON => *sized.fonts,
            _ => Fonts::new(&self.config.font_families, typo).expect("no usable font"),
        };
        fonts.set_builtin_box_drawing(self.config.builtin_box_drawing);
        fonts.set_text_antialias(self.effective_antialias());
        fonts.set_hinting(self.config.text_hinting);
        fonts.set_grid_antialias(self.config.text_antialias);
        let metrics = fonts.cell_metrics();
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "fonts ready");

        // SPAWN THE SHELL BEFORE THE GPU.
        //
        // Bringing up the GPU is ~850ms and starting pwsh is ~400ms, and neither
        // needs the other. Doing them in sequence means the prompt only starts
        // arriving once the GPU is finished, so the first frame is empty and the
        // prompt appears later still. Started here, the shell is running while
        // the driver initializes and its prompt is usually already waiting by
        // the time there is anything to draw it with.
        //
        // The grid size comes from the window and the font metrics, both of
        // which are known now -- it never needed the GPU.
        let size = window.inner_size();
        let insets = self.insets_at(scale);
        let (cols, rows) = insets.grid_dims(metrics, size.width.max(1), size.height.max(1));

        let proxy = self.proxy.clone();
        let (spec, _) = self.build_spec(None);
        // TERM, COLORTERM and the TERM_PROGRAM pair come from
        // `zest_pty::terminal_env`, which `default_shell` already applied --
        // deliberately in one place, because a child that learns the wrong
        // terminal identity produces a monochrome prompt that looks like a
        // renderer bug.

        // Find or spawn this machine's daemon and attach to it, falling back to
        // an in-process pty. This slot -- after the window is visible and the
        // first paint is measured, before GPU init -- is the one ADR-007 names,
        // and nothing above line 649 may move below it.
        // The synchronous slot fits exactly one attach, and only a local one
        // keeps the startup budget honest — everything else arrives in the
        // background. Which tab that is was the process's call
        // (`windows_state::split_lead`).
        let (restore_active, restore_rest, adopted, inherited) = match plan.first_tab {
            FirstTab::Attach { restore, rest } => (restore, rest, None, None),
            FirstTab::Inherit { route, identity, open } => {
                (None, Vec::new(), None, Some((route, identity, open)))
            }
            FirstTab::Adopt { tab, route, identity } => {
                // The new window is on the tab's host from the first moment,
                // so ⌘T and a split there reach the same daemon the tab did.
                self.route = route;
                self.client_identity = identity;
                (None, Vec::new(), Some(*tab), None)
            }
        };

        let mut tab: Option<Tab> = match (&inherited, adopted) {
            // Opened from another window: its route and identity are already
            // proven, so the shell comes through the ordinary ⌘T path once
            // the surface exists to size it — below, after the GPU.
            (Some((route, identity, _)), _) => {
                self.route = Some(route.clone());
                self.client_identity = identity.clone();
                None
            }
            // A tab moved here whole (#501): nothing to dial, only to size.
            (None, Some(tab)) => Some(tab),
            (None, None) => match self.attach_to_daemon(cols, rows, &proxy, restore_active) {
                Some(tab) => Some(tab),
                None => {
                    let addr = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
                    let cell = Arc::new(parking_lot::Mutex::new(addr));
                    let session = Session::spawn(
                        &spec,
                        PtySize::new(cols, rows),
                        self.config.scrollback,
                        wake_for(&proxy, cell, Arc::clone(&self.activity)),
                    )
                    .expect("spawn shell");
                    tracing::debug!(
                        elapsed_ms = t0.elapsed().as_millis(),
                        cols,
                        rows,
                        "shell spawned in-process"
                    );
                    Some(Tab::in_process(session, addr, (cols, rows)))
                }
            },
        };
        if let Some(tab) = tab.as_ref() {
            tab.source().terminal().lock().set_palette(self.palette.clone());
        }

        // The surface is NOT sRGB (the resolve pass encodes), so the clear value
        // is written verbatim -- pass the theme background already in sRGB.
        let bg = self.palette.background;
        let clear = wgpu::Color {
            r: f64::from(bg.r) / 255.0,
            g: f64::from(bg.g) / 255.0,
            b: f64::from(bg.b) / 255.0,
            a: f64::from(self.config.opacity),
        };
        // One device for every window (#505): the first window brings it up
        // and every later one only makes a surface on it. A surface the
        // shared adapter cannot present to — a window on another GPU — gets
        // a private device through the old path, so the ladder still ends in
        // a window rather than a panic.
        let want_transparency = self.config.translucent_surface();
        let antialias = self.effective_antialias();
        let gpu = match self.shared.gpu.get() {
            Some(host) => host.surface_for(&window, None, want_transparency, clear, antialias),
            None => pollster::block_on(GpuHost::new(&window)).and_then(|(host, surface)| {
                // Stored first, used through the cell: this window draws with
                // the host every later window will find, never a twin of it.
                // The cell was empty a moment ago on this same thread, so
                // `set` cannot fail; if it ever did, the surface below is on
                // a different instance and `surface_for` refuses it, which
                // lands this window on a private device and says so.
                if self.shared.gpu.set(host).is_err() {
                    tracing::error!("a second GPU host was brought up; the first one stays");
                }
                self.shared.gpu.get().and_then(|host| {
                    host.surface_for(&window, Some(surface), want_transparency, clear, antialias)
                })
            }),
        };
        let shared_device = gpu.is_some();
        let gpu = match gpu {
            Some(gpu) => gpu,
            None => pollster::block_on(init_gpu(&window, want_transparency, clear, antialias)),
        };
        tracing::debug!(
            elapsed_ms = t0.elapsed().as_millis(),
            shared_device,
            "gpu ready"
        );
        // The renderer may have refused subpixel because the device cannot
        // blend per channel. The rasterizer follows it, never the config —
        // see `sync_antialias` for what going the other way costs.
        fonts.set_text_antialias(gpu.renderer.text_antialias());
        fonts.set_hinting(self.config.text_hinting);
        fonts.set_grid_antialias(self.config.text_antialias);

        // The surface may have landed on a slightly different size than the
        // window reported, so reconcile before the first frame.
        let (gpu_cols, gpu_rows) = insets.grid_dims(metrics, gpu.config.width, gpu.config.height);
        // Against what the tab was last told, not against `cols`/`rows`:
        // a fresh tab was told exactly those, and an adopted one (#501) was
        // told its old window's.
        if let Some(tab) = tab.as_mut() {
            if tab.sized != (gpu_cols, gpu_rows) {
                tab.source().resize(gpu_cols, gpu_rows);
                tab.sized = (gpu_cols, gpu_rows);
            }
        }

        self.fonts = Some(fonts);
        self.gpu = Some(gpu);
        // Everything downstream of this line works the same whether the shell
        // is in this process or on another machine, which is the property the
        // abstraction exists for.
        if let Some(tab) = tab {
            self.tabs.push(tab);
        }
        // The rest of the remembered set, off the startup path. Parallel
        // workers, so one sleeping host cannot serialize the others behind
        // its timeout; arrival order may differ from the saved order, which
        // a background tab can afford.
        for saved in &restore_rest {
            let route = if saved.local {
                HostRoute::LocalSocket(zest_daemon::default_socket_path())
            } else {
                match saved.dial_hint.clone() {
                    Some(addr) => HostRoute::Tcp(addr),
                    None => {
                        tracing::warn!(addr = %saved.addr, "no way to dial a remembered host; skipping");
                        continue;
                    }
                }
            };
            let expect = (!saved.local).then_some(saved.addr.host);
            self.spawn_tab_worker_pinned(route, Some(saved.addr), expect, false, None);
        }
        self.window = Some(window);
        if let Some((_, _, open)) = &inherited {
            self.open_tab(open);
        }

        // `--screen`: dispatched here — window and session exist, the first
        // real frame has not been built — so the frame a screenshot captures
        // (and the first one a user sees) is already the asked-for surface,
        // never the terminal with a screen flashed over it. Each arm is the
        // exact call the keyboard makes, so the flag can never show a state
        // the user could not have reached.
        match self.start_screen {
            Some(StartScreen::Fleet) => self.show_screen(AppScreen::Fleet),
            Some(StartScreen::Themes) => self.show_screen(AppScreen::Themes),
            Some(StartScreen::Settings) => self.open_settings_tab(),
            Some(StartScreen::Palette) => {
                self.toggle_picker();
                // A picture of the palette is a picture of its Blocks group,
                // and at first paint an ordinary shell has run nothing. Say
                // so rather than photograph a complete-looking palette with
                // no history (#236's rule); a transcript fed through `-e`
                // that emits the OSC 133 markers is how to get one.
                let ran_anything = self.tabs.iter().any(|t| {
                    let term = t.source().terminal();
                    let term = term.lock();
                    term.blocks().blocks().iter().any(zest_core::Block::is_command)
                });
                if !ran_anything {
                    tracing::warn!(
                        "--screen palette: no command has run in this session yet, so the \
                         picture shows the palette without a Blocks group"
                    );
                }
            }
            Some(StartScreen::DirPicker) => {
                // The session's shell has not reported a cwd yet this early;
                // the process's own is the honest stand-in, and in-process
                // is exactly whose filesystem the listing reads.
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "/".to_string());
                self.open_dir_picker(cwd);
            }
            Some(StartScreen::OpenFile) => self.open_file_prompt(),
            Some(StartScreen::Find) => {
                self.toggle_find();
                if let Some(field) = self.find.as_mut() {
                    field.set("the");
                }
                self.run_find();
                // Said out loud rather than photographed silently: a bar over a
                // grid with nothing matching is a picture of the empty state
                // wearing the full state's clothes, and #236's rule is that a
                // screen which cannot be rendered honestly says so.
                if self.find_state.hits.is_empty() {
                    tracing::warn!(
                        "--screen find: nothing in this session matches the seeded query, so \
                         the picture shows the no-results state"
                    );
                }
            }
            Some(StartScreen::Editor) => {
                // A real file from wherever the binary was run, read through
                // the in-process session's own host — which under
                // `--screenshot` is this machine, so the picture has content
                // without a daemon in it.
                self.open_file_pane("README.md");
            }
            // Over the default screen, exactly as clicking the + would.
            Some(StartScreen::Launcher) => self.toggle_launcher(),
            Some(StartScreen::Profiles) => self.open_profiles_tab(),
            Some(StartScreen::ProfilesRename) => {
                self.open_profiles_tab();
                // Rail row 0 is Defaults, which is not renameable — so this
                // needs a real profile and says so when there is none, rather
                // than photographing the resting screen and looking fixed.
                if crate::profiles_ui::rail_names(&self.settings).len() > 1 {
                    self.profiles_select_rail(1);
                    self.profiles_begin_rename();
                } else {
                    tracing::warn!(
                        "--screen profiles-rename needs a profile to rename; \
                         this config has only Defaults"
                    );
                }
            }
            Some(StartScreen::SettingsMenu) => {
                self.open_settings_tab();
                // The theme row lives under Appearance, and its index does
                // not exist until the rows are built — so the category moves
                // now and the menu opens on the pass that has them.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.category = "Appearance".to_string();
                }
                self.start_menu_key = Some("appearance.theme".to_string());
            }
            None => {}
        }

        // The window is already visible and painted with the theme background
        // (see init_gpu). Present the first real frame on top of it.
        //
        // Pre-warming the atlas here matters too: without it the first frame
        // that actually contains text pays for rasterizing the whole prompt,
        // which lands as a visible hitch right after the window appears.
        self.prewarm_atlas();
        self.redraw();

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }

        tracing::info!(
            cols,
            rows,
            scale,
            startup_ms = t0.elapsed().as_millis(),
            origin = ?self.tabs.active_source().map_or(Origin::InProcess, |s| s.origin()),
            "zesterm ready"
        );

    }

    /// A wakeup from a parser thread, a worker or a watcher, already routed
    /// to this window by the process.
    pub(crate) fn handle_wakeup(&mut self, event: Wakeup) {
        match event {
            Wakeup::Redraw => {
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // Routed to the process, which is what exits (`process::route`).
            Wakeup::Exited => {}
            // The process's to place (`Process::open_requested`).
            Wakeup::OpenRequested => {}
            Wakeup::Attention(addr, cause) => self.note_attention(addr, cause),
            Wakeup::SignalChanged => self.mark_chrome_dirty(),
            // The active source parked the answer; a stale or unsolicited
            // one falls out in `apply_dir_listing`'s path comparison.
            Wakeup::DirListingReady => {
                let listing = self.tabs.active_source().and_then(|s| s.take_dir_listing());
                if let Some(listing) = listing {
                    self.apply_dir_listing(listing);
                }
            }
            Wakeup::FileContentsReady => {
                self.drain_file_replies();
            }
            // The link died, not the shell. The window stays open showing the
            // last state that was true -- closing it would throw away a session
            // that is still running in a daemon that does not care we went
            // away, which is the property ADR-007 exists to provide.
            Wakeup::Detached => {
                tracing::warn!("the daemon connection dropped; the session is still running");
                // Nothing to schedule here: `RemoteSession` supervises its own
                // link and is already dialling. The window goes on showing the
                // last state that was true, because the session still exists —
                // and the status bar says "reconnecting" until it is.
                self.link_down = true;
                self.mark_chrome_dirty();
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // The in-place reconnect in `remote.rs` succeeded: same session, same
            // grid, and everything this window had accumulated is still there.
            // Nothing to rebuild — just repaint.
            // A worker finished opening a tab; adopt everything it parked.
            Wakeup::TabsChanged => {
                let mut fresh = self.pending_tabs.lock();
                let tabs: Vec<(Tab, bool)> = fresh.drain(..).collect();
                drop(fresh);
                let pushed = !tabs.is_empty();
                for (mut tab, focus) in tabs {
                    // A command riding this tab (§6's `⇧⏎ run on host…`): the
                    // session it needed now exists. Taken *before* `adopt`,
                    // because a refused duplicate is dropped and would take
                    // the command with it — and keyed to this address, so it
                    // reaches the session the user chose rather than whatever
                    // is active when a background dial happens to land.
                    let run = tab.take_pending_input();
                    let addr = tab.addr;
                    // Refused duplicates (#188) detach on drop — the shell
                    // stays on its host; the strip already activated the
                    // tab that holds it. This is also what heals a
                    // tabs.json that persisted a duplicate: the restore's
                    // second copy dies here on every launch.
                    if let Some(dup) = self.tabs.adopt(tab, focus) {
                        // Accurate about focus: a background restore's
                        // duplicate is refused without touching the keyboard.
                        tracing::info!(addr = %dup.addr, focus, "session already open; refusing the duplicate");
                        drop(dup);
                    }
                    // The duplicate case still runs it: the strip holds that
                    // session under another tab, and "run this there" was
                    // about the machine and the session, never about which
                    // tab object won.
                    if let Some(run) = run {
                        match self.tabs.find_mut(addr) {
                            Some(tab) => tab.source().write(run),
                            // Closed between the dial landing and now. Say so
                            // rather than dropping it silently — a command
                            // that goes nowhere quietly is worse than one that
                            // fails loudly.
                            None => tracing::warn!(
                                %addr,
                                "the tab closed before its command could run"
                            ),
                        }
                    }
                }
                // Profile launches settling (issue #175): the connecting tab
                // is already in the strip, so this swaps its session in (or
                // marks it dead carrying the error) rather than pushing.
                let settled: Vec<_> = self.pending_launches.lock().drain(..).collect();
                let dims = self.current_dims();
                let mut settled_pane = false;
                for (placeholder, outcome) in settled {
                    match outcome {
                        Ok(session) => match self.tabs.find_mut(placeholder) {
                            Some(tab) => {
                                tab.resolve_live(session, false);
                                // The window may have resized while the dial
                                // was in flight, and the lazy-resize path
                                // only catches *activation* — this tab is
                                // most likely already the active one.
                                if tab.sized != dims {
                                    tab.source().resize(dims.0, dims.1);
                                    tab.sized = dims;
                                }
                                tab.source().mark_dirty();
                            }
                            // A pane's dial (#436): same swap, in its frame.
                            // The fit is re-run below — panes are sized by
                            // their column, not the grid.
                            None => match self.tabs.find_pane_owner(placeholder) {
                                Some(tab) => {
                                    let local = matches!(
                                        crate::source::SessionSource::origin(&session),
                                        Origin::Daemon { local: true, .. }
                                    );
                                    if let Some(pane) =
                                        tab.panes.iter_mut().find(|p| p.addr == placeholder)
                                    {
                                        pane.resolve_live(session, local);
                                        if let Some(s) = pane.session() {
                                            s.mark_dirty();
                                        }
                                    }
                                    settled_pane = true;
                                }
                                // Closed while dialling: dropping detaches,
                                // the shell stays on its host for the picker
                                // to find.
                                None => drop(session),
                            },
                        },
                        Err(error) => {
                            tracing::warn!(%placeholder, error, "profile launch failed");
                            if let Some(tab) = self.tabs.find_mut(placeholder) {
                                tab.resolve_failed(&error);
                            } else if let Some(tab) = self.tabs.find_pane_owner(placeholder) {
                                if let Some(pane) =
                                    tab.panes.iter_mut().find(|p| p.addr == placeholder)
                                {
                                    pane.resolve_failed(&error);
                                }
                            }
                        }
                    }
                }
                if settled_pane {
                    // The window may have resized while the dial was in
                    // flight, and `sized` still says what the placeholder was
                    // told, so only a pane whose column moved pays a resize.
                    self.resize_split_panes();
                }
                if pushed {
                    // A worker-opened tab takes the keyboard, so this is an
                    // activation. A settling launch is not: its tab was
                    // activated when it was pushed, and after_activation()
                    // here would yank a full-pane screen out from under the
                    // user because a background dial finished.
                    self.after_activation();
                    self.relayout_grid();
                } else {
                    self.mark_chrome_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
                self.requests.persist = true;
            }
            // The latch is the process's to consume (`process::route`): it
            // clears once, and the process tells every window.
            Wakeup::FleetChanged => {}
            // An account worker settled; adopt what it parked. The fleet
            // header is part of the cached chrome, so this is a rebuild.
            Wakeup::AccountChanged => {
                // Taken before the assignment: the guard's temporary borrows
                // `self` for the whole `if let` otherwise.
                // The approve workers' channel rides the same wakeup; drain
                // it first so a failure and the state that caused it land
                // in one chrome rebuild — and note it separately, because an
                // approval outcome often arrives with the account cell empty
                // and must still repaint the section.
                let approve_settled = self.devices_error_update.lock().take();
                if let Some(error) = approve_settled {
                    self.devices_error = error;
                    self.mark_chrome_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
                let enroll_settled = self.local_enroll_update.lock().take();
                if let Some(state) = enroll_settled {
                    let enrolled = matches!(state, LocalEnroll::Enrolled { .. });
                    self.local_enroll = state;
                    self.mark_chrome_dirty();
                    if enrolled {
                        // The listing is what the card ultimately draws
                        // from; hurry it so the account's own `enrolled` row
                        // replaces the worker's transient message.
                        if let Some(poke) = self.account_poke.as_ref() {
                            poke.poke();
                        }
                    }
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
                let settled = self.account_update.lock().take();
                if let Some(state) = settled {
                    // Poke only on a *transition*: the watcher itself posts
                    // SignedOut on a 401, and poking on that re-adoption
                    // would fetch, 401, post, adopt, poke — a loop at
                    // network round-trip cadence.
                    let moved = state != self.account;
                    self.account = state;
                    if moved
                        && matches!(
                            self.account,
                            AccountState::SignedIn { .. } | AccountState::SignedOut
                        )
                    {
                        // Sign-in resumes a parked watcher; sign-out makes
                        // it clear the listing now rather than a minute on.
                        if let Some(poke) = self.account_poke.as_ref() {
                            poke.poke();
                        }
                    }
                    self.mark_chrome_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }
            // One tab's child exited. Close that tab — killing is moot, the
            // child is already gone — and the last tab closing closes the
            // window, which is exactly the old single-session behavior.
            Wakeup::TabExited(addr) => {
                // A split pane's shell ending collapses the pane, never the
                // tab it lived in.
                if self.tabs.find_pane_owner(addr).is_some_and(|t| t.remove_gone_pane(addr)) {
                    self.relayout_grid();
                    self.mark_chrome_dirty();
                    return;
                }
                self.close_tab(addr, true);
            }
            // A pinned tab's host answered and its session no longer exists.
            // The prompt itself travels in the shared pairing cell; this
            // event only asks for the chrome to be rebuilt around it.
            Wakeup::PairingChanged => self.mark_chrome_dirty(),
            // The supervisor stopped rather than swapping in a fresh shell;
            // the tab stays put, marked ended, until the user closes it (a
            // recreate affordance arrives with the picker).
            Wakeup::SessionGone(addr) => {
                tracing::warn!(%addr, "the session ended on its host");
                // A redial can pend, be approved, and then find the session
                // gone — the supervisor stops there, so nothing later would
                // clear the prompt it latched.
                self.pairing.lock().take();
                if let Some(tab) = self.tabs.find_mut(addr) {
                    tab.dead = true;
                } else if let Some(tab) = self.tabs.find_pane_owner(addr) {
                    // The pane stays put showing its last state, like a dead
                    // tab does — vanishing mid-glance is worse.
                    if let Some(pane) = tab.panes.iter_mut().find(|p| p.addr == addr) {
                        pane.dead = true;
                    }
                }
                self.mark_chrome_dirty();
            }
            Wakeup::Reattached => {
                tracing::info!("the daemon connection is back");
                self.link_down = false;
                // A redial that pended was answered — that is how the link
                // came back — so the prompt is settled either way.
                self.pairing.lock().take();
                self.mark_chrome_dirty();
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Wakeup::ConfigChanged => self.reload_config(),
        }
    }


    pub(crate) fn handle_window_event(&mut self, event: WindowEvent) {
        match event {
            // Detach, never close. The session keeps running in the daemon and
            // can be picked up from another window or another device -- which
            // is the whole payoff of ADR-007, and is lost the moment closing a
            // window is allowed to mean "end the shell".
            //
            // Dropping the session is what sends the Detach: a destructor
            // covers every way this process can end, including the ones no
            // `CloseRequested` arm would see.
            WindowEvent::CloseRequested => self.request_close(),

            WindowEvent::Resized(size) => self.resize_surface(size.width, size.height),

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // A DPI change invalidates every rasterized glyph, so bump the
                // atlas generation and recompute geometry. Doing this in two
                // steps would render a frame at the wrong size.
                if let Some(fonts) = self.fonts.as_mut() {
                    fonts.set_typography(Typography {
                        scale_factor: scale_factor as f32,
                        ..self.config.typography
                    });
                }
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.renderer.atlas.clear();
                }
                if let Some(w) = self.window.as_ref() {
                    let size = w.inner_size();
                    self.resize_surface(size.width, size.height);
                }
            }

            WindowEvent::ThemeChanged(theme) => {
                let light = theme == winit::window::Theme::Light;
                if self.system_light == light {
                    return;
                }
                self.system_light = light;
                // Tracked even when not following, so turning the setting on
                // later is correct immediately. Only *repainting* is
                // conditional — re-resolving the theme on a window that is not
                // following would be work with no visible result, on an event
                // that arrives for every window on the desktop at once.
                if self.config.follow_system_theme {
                    self.apply_theme();
                    if let Some(session) = self.tabs.active_source() {
                        session.mark_dirty();
                    }
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::Focused(focused) => {
                self.focused = focused;
                // Coming back to the window is looking at its active tab —
                // the other half of `note_attention`'s rule, which counts a
                // signal as unseen while the window is behind something else.
                if focused {
                    if let Some(addr) = self.tabs.active().map(|t| t.addr) {
                        self.clear_attention(addr);
                    }
                }
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                // The active tab's label dims with the window, like any
                // native titlebar.
                self.mark_chrome_dirty();
            }

            WindowEvent::Occluded(false) => {
                // Frames attempted while occluded were skipped with their
                // damage re-armed; this is the moment they were waiting for.
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            // One event per file, and none of the three carries a position --
            // see `drop_target` for what that costs and what is done instead.
            WindowEvent::HoveredFile(path) => self.on_file_hovered(&path),

            WindowEvent::HoveredFileCancelled => self.clear_drop_hint(),

            WindowEvent::DroppedFile(path) => self.on_file_dropped(&path),

            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),

            WindowEvent::Ime(ime) => self.on_ime(ime),

            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if event.state != ElementState::Pressed {
                    // A release exists only for a program that asked for kitty
                    // event types, and it must not walk the rest of this arm:
                    // every chord below fires on press, so a binding that also
                    // matched a release would run twice.
                    self.on_key_release(&event);
                    return;
                }
                // While an input method is composing, the keys are its own:
                // `Enter` picks a candidate rather than running a command, and
                // the arrows move through the candidate list. winit documents
                // that it withholds these events during a preedit, but that is a
                // promise made by four backends rather than a property of this
                // code, and the failure mode -- a half-composed word running as
                // a command -- is bad enough to check twice.
                if self.ime.composing() {
                    return;
                }

                // The approval modal owns exactly one key: Esc dismisses
                // it, deciding nothing — the daemon times out the request as
                // it always has. Every other key falls through, because the
                // modal pops up on its own schedule and must not eat the
                // keystrokes of whoever was mid-command underneath it.
                {
                    use winit::keyboard::{Key, NamedKey};
                    if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                        let dismissed = {
                            let mut queue = self.approval.lock();
                            match visible_approval(&queue, std::time::Instant::now()) {
                                Some(i) => {
                                    // The next queued request (if any) shows
                                    // in this one's place on the repaint.
                                    queue[i].dismissed = true;
                                    true
                                }
                                None => false,
                            }
                        };
                        if dismissed {
                            self.mark_chrome_dirty();
                            return;
                        }
                    }
                }

                // The close confirm owns the keyboard outright, which the
                // approval modal above deliberately does not: that one opens
                // on the network's schedule over whoever was mid-command, so
                // it may not eat their keystrokes. This one opened *because*
                // of a keystroke, and letting the next one through to a shell
                // the question is about to end would be the worse mistake.
                if self.confirm_close.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => self.answer_confirm_close(None),
                        // Enter takes the answer that destroys nothing, and
                        // takes it only when there is one — a reflexive Enter
                        // after ⌘W must never be the thing that kills a
                        // build.
                        Key::Named(NamedKey::Enter)
                            if self.confirm_close.as_ref().is_some_and(|c| {
                                c.choices == crate::chrome::model::ConfirmChoices::DetachOrClose
                            }) =>
                        {
                            self.answer_confirm_close(Some(false));
                        }
                        _ => {}
                    }
                    return;
                }

                // The open block menu owns the keyboard, on the launcher's
                // rules. Chords resolve through the table first, so ⌘⇧O with
                // a menu open copies the block it is about and dismisses —
                // coherent, because opening the menu selected that block and
                // the chords now target the selection.
                if self.block_menu.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        self.block_menu = None;
                        self.mark_chrome_dirty();
                        self.perform(binding.action);
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            // Only the menu. A lit block is *not* cleared by
                            // Esc — that key belongs to the shell (vim, less,
                            // readline), and swallowing it because a block is
                            // selected is a regression felt within a minute.
                            self.block_menu = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let action = self
                                .block_menu
                                .as_ref()
                                .and_then(|m| m.actions.get(m.selected).copied());
                            if let Some(action) = action {
                                self.run_block_menu_action(action);
                            }
                        }
                        Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp) => {
                            let down =
                                matches!(&event.logical_key, Key::Named(NamedKey::ArrowDown));
                            if let Some(m) = self.block_menu.as_mut() {
                                m.selected =
                                    crate::block_menu::step(&m.actions, m.selected, down);
                            }
                            self.mark_chrome_dirty();
                        }
                        _ => {}
                    }
                    return;
                }
                // The open launcher owns the keyboard, like every overlay.
                // Chords resolve through the table first: ⌘1..⌘9 stay
                // ActivateTab (the plain digits below are the launcher's),
                // ⌘K switches to the picker, ⌘T spawns the default — the
                // menu yields to any chord rather than dying against it.
                if self.launcher.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        self.launcher = None;
                        self.mark_chrome_dirty();
                        self.perform(binding.action);
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.launcher = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            // ⇧⏎ is the Run-on-another-host chord wherever
                            // the selection sits; plain ⏎ runs the selected
                            // row — the default row, until the user moves.
                            if self.modifiers.shift_key() {
                                // Whatever is highlighted rides along; the
                                // builder cannot know where the selection is,
                                // so the chord fills it in here.
                                let target = self.launcher.as_ref().and_then(|l| {
                                    match l.actions.get(l.selected) {
                                        Some(crate::launcher::LauncherAction::Launch(t)) => {
                                            Some(t.clone())
                                        }
                                        _ => None,
                                    }
                                });
                                self.run_launcher_action(
                                    crate::launcher::LauncherAction::RunOnHost(target),
                                );
                            } else {
                                let action = self
                                    .launcher
                                    .as_ref()
                                    .and_then(|l| l.actions.get(l.selected).cloned());
                                if let Some(action) = action {
                                    self.run_launcher_action(action);
                                }
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(l) = self.launcher.as_mut() {
                                l.selected = crate::launcher::step(&l.actions, l.selected, true);
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(l) = self.launcher.as_mut() {
                                l.selected = crate::launcher::step(&l.actions, l.selected, false);
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Character(c) => {
                            // Plain digits 1–9 run the Nth profile row (§1's
                            // ⌘N hints, minus the modifier the open menu
                            // makes unnecessary). Chords were resolved
                            // above, so anything with a desktop modifier is
                            // already gone.
                            let digit = (!self.modifiers.control_key()
                                && !key::belongs_to_desktop(self.modifiers))
                            .then(|| c.as_str())
                            .and_then(|s| s.parse::<u8>().ok())
                            .filter(|d| (1..=9).contains(d));
                            if let Some(d) = digit {
                                let action = self.launcher.as_ref().and_then(|l| {
                                    let i = crate::launcher::digit_action_index(&l.actions, d)?;
                                    l.actions.get(i).cloned()
                                });
                                if let Some(action) = action {
                                    self.run_launcher_action(action);
                                }
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // The open picker owns the keyboard entirely: a keystroke
                // meant for a list must never reach a shell.
                if self.picker.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    // The two chords that switch overlays rather than dying
                    // against the modal wall. Resolved through the binding
                    // table like everything else, because a character
                    // comparison here could only ever match one of the two
                    // spellings: ⌘, arrives as `Character(",")` while
                    // Ctrl+Shift+, arrives as `"<"`.
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        match binding.action {
                            keymap::Action::ToggleFleetPicker => {
                                self.toggle_picker();
                                return;
                            }
                            keymap::Action::ToggleSettings => {
                                self.open_settings_tab();
                                return;
                            }
                            _ => {}
                        }
                    }
                    // The filter is a text field: typing, ⌫, the arrows, and
                    // every clipboard chord go through one place (#251).
                    // Consulted after the overlay-switching chords above and
                    // before the list's own keys, which `command_for`
                    // declines — Enter, Escape, ↑/↓.
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        let pasted = self.paste_text(&cmd);
                        let mut copied = None;
                        let mut asked = None;
                        if let Some(p) = self.picker.as_mut() {
                            let out = p.filter.apply(cmd, pasted.as_deref());
                            if out.changed {
                                p.selected = 0;
                                asked = Some(p.filter.text().to_string());
                            }
                            copied = out.copied;
                        }
                        // Only a *changed* filter asks the fleet again — an
                        // arrow or a copy chord leaves the answers standing.
                        if let Some(query) = asked {
                            if let Some(fleet) = self.shared.fleet.get() {
                                fleet.search_blocks(&query);
                            }
                        }
                        if let Some(text) = copied {
                            self.set_clipboard(text);
                        }
                        self.mark_chrome_dirty();
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.picker = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.picker.as_mut() {
                                // Skip group labels: the selection lands on
                                // things Enter can do, never on a heading.
                                let mut next = p.selected;
                                while next + 1 < p.actions.len() {
                                    next += 1;
                                    if !matches!(p.actions[next], PickerAction::None) {
                                        p.selected = next;
                                        break;
                                    }
                                }
                                p.scroll_to_selected = true;
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.picker.as_mut() {
                                let mut next = p.selected;
                                while next > 0 {
                                    next -= 1;
                                    if !matches!(p.actions[next], PickerAction::None) {
                                        p.selected = next;
                                        break;
                                    }
                                }
                                p.scroll_to_selected = true;
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let action = self
                                .picker
                                .as_ref()
                                .and_then(|p| p.actions.get(p.selected).cloned());
                            if let Some(action) = action {
                                let shift = self.modifiers.shift_key();
                                self.run_picker_action(action, shift);
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // The find bar takes the keys that are *text*, and only those.
                //
                // It is the one overlay that is not exclusive with the others,
                // so it must not own the keyboard the way they do: swallowing
                // everything here would stop ⌘K opening the palette over it and
                // would make ⌘F on an open bar do nothing, when it is supposed
                // to re-select the query. `command_for` first — the rule every
                // text entry here follows (#228/#251/#270), so ⌘V reaches the
                // field — then the chord table, on the block menu's pattern
                // twenty lines up, except that a chord leaves the bar *open*:
                // it is a search still in progress, not a menu being dismissed.
                if self.find.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        let pasted = self.paste_text(&cmd);
                        let mut copied = None;
                        if let Some(field) = self.find.as_mut() {
                            copied = field.apply(cmd, pasted.as_deref()).copied;
                        }
                        if let Some(text) = copied {
                            self.set_clipboard(text);
                        }
                        self.run_find();
                        self.mark_chrome_dirty();
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                        return;
                    }
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        self.perform(binding.action);
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.close_find();
                        }
                        // ⏎ / ⇧⏎ rather than a chord: ⌘G is spent by Open
                        // file… and ⌘⇧F folds onto ⌘F's own Windows spelling,
                        // so there is no letter left for find-next. See
                        // `Action::ToggleFind`.
                        Key::Named(NamedKey::Enter) => {
                            let back = self.modifiers.shift_key();
                            self.step_find(if back { -1 } else { 1 });
                        }
                        _ => {}
                    }
                    return;
                }

                // The "Open file…" prompt owns the keyboard while it is up.
                // `command_for` is consulted **first** — the rule every text
                // entry in this app follows (#228/#251/#270): a field that
                // handles its own keys is a field that eats ⌘V.
                if self.open_file.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        let pasted = self.paste_text(&cmd);
                        let mut copied = None;
                        if let Some(field) = self.open_file.as_mut() {
                            copied = field.apply(cmd, pasted.as_deref()).copied;
                        }
                        if let Some(text) = copied {
                            self.set_clipboard(text);
                        }
                        self.mark_chrome_dirty();
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.open_file = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let path = self
                                .open_file
                                .as_ref()
                                .map(|f| f.text().trim().to_string())
                                .unwrap_or_default();
                            // An empty path is not a refusal to report — it is
                            // someone changing their mind, and closing is what
                            // they meant.
                            self.open_file = None;
                            self.mark_chrome_dirty();
                            if !path.is_empty() {
                                self.open_file_pane(&path);
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // The open command palette likewise owns the keyboard. It and
                // the picker are mutually exclusive (the toggles enforce it),
                // so the order of these blocks carries no meaning.
                if self.dir_picker.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    // The palette's rules verbatim: overlay chords outrank
                    // the filter, text goes to the filter, and the rest are
                    // the picker's own verbs.
                    match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                        .map(|b| b.action)
                    {
                        Some(keymap::Action::TogglePalette) => {
                            self.dir_picker = None;
                            self.toggle_palette();
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(keymap::Action::ToggleSettings) => {
                            self.dir_picker = None;
                            self.open_settings_tab();
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(keymap::Action::ToggleFleetPicker) => {
                            self.dir_picker = None;
                            self.toggle_picker();
                            self.mark_chrome_dirty();
                            return;
                        }
                        _ => {}
                    }
                    // Tab descends — an *un-typeable* browse verb on purpose:
                    // the arrows belong to the filter's caret and the rows'
                    // selection, and Enter is the switch itself.
                    if matches!(&event.logical_key, Key::Named(NamedKey::Tab)) {
                        let i = self.dir_picker.as_ref().map_or(0, |p| p.selected);
                        self.dir_picker_descend(i);
                        return;
                    }
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        let pasted = self.paste_text(&cmd);
                        let mut copied = None;
                        if let Some(p) = self.dir_picker.as_mut() {
                            let out = p.filter.apply(cmd, pasted.as_deref());
                            if out.changed {
                                p.selected = 0;
                                p.scroll_to_selected = true;
                            }
                            copied = out.copied;
                        }
                        if let Some(text) = copied {
                            self.set_clipboard(text);
                        }
                        self.mark_chrome_dirty();
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.dir_picker = None;
                        }
                        Key::Named(NamedKey::Enter) => {
                            let i = self.dir_picker.as_ref().map_or(0, |p| p.selected);
                            self.dir_picker_activate(i);
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.dir_picker.as_mut() {
                                let last = p.rows.len().saturating_sub(1);
                                p.selected = (p.selected + 1).min(last);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.dir_picker.as_mut() {
                                p.selected = p.selected.saturating_sub(1);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::PageDown) => {
                            if let Some(p) = self.dir_picker.as_mut() {
                                p.scroll += 300.0;
                            }
                        }
                        Key::Named(NamedKey::PageUp) => {
                            if let Some(p) = self.dir_picker.as_mut() {
                                p.scroll -= 300.0;
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                if self.palette_ui.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    // The overlay-switching chords outrank the filter — the
                    // opening chord closes too, aliases included, resolved
                    // through the table so ⌘/, ⌘? and ⌘⇧P agree. Ahead of
                    // `command_for`, or ⌘X would cut instead of switching.
                    match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                        .map(|b| b.action)
                    {
                        Some(keymap::Action::TogglePalette) => {
                            self.palette_ui = None;
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(keymap::Action::ToggleSettings) => {
                            self.open_settings_tab();
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(keymap::Action::ToggleFleetPicker) => {
                            self.toggle_picker();
                            self.mark_chrome_dirty();
                            return;
                        }
                        _ => {}
                    }
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        let pasted = self.paste_text(&cmd);
                        let mut copied = None;
                        if let Some(p) = self.palette_ui.as_mut() {
                            let out = p.filter.apply(cmd, pasted.as_deref());
                            if out.changed {
                                p.selected = 0;
                                p.scroll_to_selected = true;
                            }
                            copied = out.copied;
                        }
                        if let Some(text) = copied {
                            self.set_clipboard(text);
                        }
                        self.mark_chrome_dirty();
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.palette_ui = None;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.run_palette_selection();
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.selected = keymap::step_runnable(&p.actions, p.selected, true);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.selected = keymap::step_runnable(&p.actions, p.selected, false);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::PageDown) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.scroll += 300.0;
                            }
                        }
                        Key::Named(NamedKey::PageUp) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.scroll -= 300.0;
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // And the Settings tab, while it holds the grid area — the
                // full-pane screens (Esc returns) are checked below, so a
                // fleet view open over the tab keeps its own keys.
                if self.settings_tab_active() && self.screen == AppScreen::Terminal {
                    use winit::keyboard::{Key, NamedKey};

                    // The open dropdown menu owns the keys before everything.
                    if self.settings_ui.as_ref().is_some_and(|ui| ui.menu.is_some()) {
                        // How many rows the menu is *showing* — a live search
                        // has already narrowed a roster, and clamping the
                        // selection against the unfiltered count would let
                        // ↓ run off the end of what is drawn.
                        let options = self
                            .settings_ui
                            .as_ref()
                            .and_then(|ui| {
                                let menu = ui.menu.as_ref()?;
                                if !menu.roster.is_empty() {
                                    return Some(menu.matching().len());
                                }
                                let i = match ui.actions.get(menu.row) {
                                    Some(crate::settings_ui::RowAction::Field(i)) => *i,
                                    _ => return None,
                                };
                                ui.fields.get(i).map(|f| f.variants.len())
                            })
                            .unwrap_or(0);
                        // The search row is a text field like any other, and
                        // only on a searchable menu: on a four-variant select
                        // a stray letter must not silently start filtering
                        // something with no visible box (#259).
                        let searchable = self
                            .settings_ui
                            .as_ref()
                            .and_then(|ui| ui.menu.as_ref())
                            .is_some_and(MenuState::searchable);
                        if searchable {
                            if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                                let pasted = self.paste_text(&cmd);
                                let mut copied = None;
                                if let Some(menu) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    let out = menu.filter.apply(cmd, pasted.as_deref());
                                    if out.changed {
                                        menu.selected = 0;
                                        menu.scroll = 0.0;
                                        menu.scroll_to_selected = true;
                                    }
                                    copied = out.copied;
                                }
                                if let Some(text) = copied {
                                    self.set_clipboard(text);
                                }
                                self.mark_chrome_dirty();
                                return;
                            }
                        }
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.menu = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let sel = self
                                    .settings_ui
                                    .as_ref()
                                    .and_then(|ui| ui.menu.as_ref())
                                    .map(|m| m.selected);
                                if let Some(sel) = sel {
                                    self.apply_menu_choice(sel);
                                }
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if let Some(menu) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.selected =
                                        (menu.selected + 1).min(options.saturating_sub(1));
                                    menu.scroll_to_selected = true;
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                if let Some(menu) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.selected = menu.selected.saturating_sub(1);
                                    menu.scroll_to_selected = true;
                                }
                            }
                            Key::Named(NamedKey::PageDown) => {
                                if let Some(menu) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.scroll += 300.0;
                                    menu.scroll_to_selected = false;
                                }
                            }
                            Key::Named(NamedKey::PageUp) => {
                                if let Some(menu) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.scroll -= 300.0;
                                    menu.scroll_to_selected = false;
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // A tab is not a modal: the tab-management chords keep
                    // working over it — including ⌘W, which is how this tab
                    // closes (§11). Ahead of the text path, or ⌘X would cut
                    // the filter instead of doing whatever it is bound to.
                    match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                        .map(|b| b.action)
                    {
                        // ⌘, on the active settings tab is already where it
                        // goes; swallow rather than reopen.
                        Some(keymap::Action::ToggleSettings) => {
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(
                            action @ (keymap::Action::ToggleFleetPicker
                            | keymap::Action::TogglePalette
                            | keymap::Action::CloseTab
                            | keymap::Action::NewTab
                            | keymap::Action::ToggleTabLayout
                            | keymap::Action::ActivateTab(_)
                            | keymap::Action::ActivateLastTab
                            | keymap::Action::PrevTab
                            | keymap::Action::NextTab),
                        ) => {
                            self.perform(action);
                            self.mark_chrome_dirty();
                            return;
                        }
                        _ => {}
                    }

                    // Text before navigation: `text_key` routes to the open
                    // edit buffer when there is one and to the filter
                    // otherwise, so typing, ⌫ and every clipboard chord have
                    // one path instead of two copies that disagree (#251).
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        // With no buffer open the arrows belong to the list —
                        // ←/→ adjust the selected setting (§11), which
                        // outranks a caret in a filter nobody is inside.
                        let editing =
                            self.settings_ui.as_ref().is_some_and(|ui| ui.editing.is_some());
                        let navigation = matches!(
                            cmd,
                            TextCommand::Move { .. }
                                | TextCommand::Home { .. }
                                | TextCommand::End { .. }
                        );
                        // '/' focuses the filter (§11) — which is where every
                        // other character already goes, so focusing it is
                        // swallowing the slash. Inside a buffer it is a path
                        // separator and must type.
                        let focus_filter =
                            !editing && cmd == TextCommand::Insert("/".to_string());
                        if focus_filter {
                            self.mark_chrome_dirty();
                            return;
                        }
                        if editing || !navigation {
                            let pasted = self.paste_text(&cmd);
                            let copied = self
                                .settings_ui
                                .as_mut()
                                .and_then(|ui| ui.text_key(cmd, pasted.as_deref()));
                            if let Some(text) = copied {
                                self.set_clipboard(text);
                            }
                            self.mark_chrome_dirty();
                            return;
                        }
                    }

                    // A typed edit owns the keys before the list does — while
                    // a buffer is open, Enter commits it and Esc drops it.
                    if self.settings_ui.as_ref().is_some_and(|ui| ui.editing.is_some()) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.editing = None;
                                }
                            }
                            // Enter is now one exit among several rather
                            // than the only one that commits (#275).
                            Key::Named(NamedKey::Enter) => {
                                self.settings_commit_edit();
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            self.activate_selected_setting();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.adjust_selected_setting(1);
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.adjust_selected_setting(-1);
                        }
                        Key::Named(NamedKey::Escape) => {
                            // Layered: edit and menu were handled above, so
                            // here a filter clears first, and a second Esc
                            // CLOSES THE TAB — closing it is closing a tab.
                            let filtered = self
                                .settings_ui
                                .as_ref()
                                .is_some_and(|ui| !ui.filter.is_empty());
                            if filtered {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.filter.clear();
                                    ui.selected = 0;
                                    ui.scroll_to_selected = true;
                                }
                            } else {
                                self.close_settings_tab();
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    true,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    false,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // The Profiles editor, while its pane holds the grid area —
                // the Settings tab's keyboard discipline, on §12's surface.
                if self.profiles_tab_active() {
                    use winit::keyboard::{Key, NamedKey};

                    // The open dropdown menu owns the keys before everything.
                    if self.profiles_ui.as_ref().is_some_and(|ui| ui.menu.is_some()) {
                        // How many rows the menu is *showing* — a live search
                        // has already narrowed a roster, and clamping the
                        // selection against the unfiltered count would let
                        // ↓ run off the end of what is drawn.
                        let options = self
                            .profiles_ui
                            .as_ref()
                            .and_then(|ui| {
                                let menu = ui.menu.as_ref()?;
                                if !menu.roster.is_empty() {
                                    return Some(menu.matching().len());
                                }
                                let i = match ui.actions.get(menu.row) {
                                    Some(crate::settings_ui::RowAction::Field(i)) => *i,
                                    _ => return None,
                                };
                                ui.fields.get(i).map(|f| f.variants.len())
                            })
                            .unwrap_or(0);
                        // The search row is a text field like any other, and
                        // only on a searchable menu: on a four-variant select
                        // a stray letter must not silently start filtering
                        // something with no visible box (#259).
                        let searchable = self
                            .profiles_ui
                            .as_ref()
                            .and_then(|ui| ui.menu.as_ref())
                            .is_some_and(MenuState::searchable);
                        if searchable {
                            if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                                let pasted = self.paste_text(&cmd);
                                let mut copied = None;
                                if let Some(menu) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    let out = menu.filter.apply(cmd, pasted.as_deref());
                                    if out.changed {
                                        menu.selected = 0;
                                        menu.scroll = 0.0;
                                        menu.scroll_to_selected = true;
                                    }
                                    copied = out.copied;
                                }
                                if let Some(text) = copied {
                                    self.set_clipboard(text);
                                }
                                self.mark_chrome_dirty();
                                return;
                            }
                        }
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.menu = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let sel = self
                                    .profiles_ui
                                    .as_ref()
                                    .and_then(|ui| ui.menu.as_ref())
                                    .map(|m| m.selected);
                                if let Some(sel) = sel {
                                    self.profiles_apply_menu_choice(sel);
                                }
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if let Some(menu) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.selected =
                                        (menu.selected + 1).min(options.saturating_sub(1));
                                    menu.scroll_to_selected = true;
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                if let Some(menu) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.selected = menu.selected.saturating_sub(1);
                                    menu.scroll_to_selected = true;
                                }
                            }
                            Key::Named(NamedKey::PageDown) => {
                                if let Some(menu) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.scroll += 300.0;
                                    menu.scroll_to_selected = false;
                                }
                            }
                            Key::Named(NamedKey::PageUp) => {
                                if let Some(menu) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    menu.scroll -= 300.0;
                                    menu.scroll_to_selected = false;
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // The tab-management chords, ahead of the text path —
                    // §11's rule, taken whole (⌘W closes it via perform's
                    // Profiles arm).
                    match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                        .map(|b| b.action)
                    {
                        // ⌘⇧, on the active Profiles tab is already where it
                        // goes; swallow rather than reopen.
                        Some(keymap::Action::OpenProfiles) => {
                            self.mark_chrome_dirty();
                            return;
                        }
                        Some(
                            action @ (keymap::Action::ToggleFleetPicker
                            | keymap::Action::TogglePalette
                            | keymap::Action::ToggleSettings
                            | keymap::Action::CloseTab
                            | keymap::Action::NewTab
                            | keymap::Action::ToggleTabLayout
                            | keymap::Action::ActivateTab(_)
                            | keymap::Action::ActivateLastTab
                            | keymap::Action::PrevTab
                            | keymap::Action::NextTab),
                        ) => {
                            self.perform(action);
                            self.mark_chrome_dirty();
                            return;
                        }
                        _ => {}
                    }

                    // §12's text path, the Settings tab's exactly (#251).
                    if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                        // The name entry counts as a buffer for every
                        // routing decision below: inside it a digit is text,
                        // '/' is text, and ←/→ move its caret rather than the
                        // row selection (#283).
                        let editing = self.profiles_ui.as_ref().is_some_and(|ui| {
                            ui.editing.is_some() || ui.renaming.is_some()
                        });
                        // A LEADING digit jumps the rail (its drawn 1–9
                        // hints); once a filter is live, digits filter like
                        // any other character — the two meanings are
                        // separated by the filter's emptiness, not by
                        // guesswork. Inside a buffer a digit is always text.
                        let rail_jump = (!editing)
                            .then(|| match &cmd {
                                TextCommand::Insert(s) => s.parse::<usize>().ok(),
                                _ => None,
                            })
                            .flatten()
                            .filter(|d| (1..=9).contains(d))
                            .filter(|_| {
                                self.profiles_ui.as_ref().is_some_and(|ui| ui.filter.is_empty())
                            });
                        if let Some(d) = rail_jump {
                            self.profiles_select_rail(d);
                            self.mark_chrome_dirty();
                            return;
                        }
                        // '/' focuses the filter — where every other
                        // character already goes (§11).
                        if !editing && cmd == TextCommand::Insert("/".to_string()) {
                            self.mark_chrome_dirty();
                            return;
                        }
                        let navigation = matches!(
                            cmd,
                            TextCommand::Move { .. }
                                | TextCommand::Home { .. }
                                | TextCommand::End { .. }
                        );
                        if editing || !navigation {
                            let pasted = self.paste_text(&cmd);
                            let copied = self
                                .profiles_ui
                                .as_mut()
                                .and_then(|ui| ui.text_key(cmd, pasted.as_deref()));
                            if let Some(text) = copied {
                                self.set_clipboard(text);
                            }
                            self.mark_chrome_dirty();
                            return;
                        }
                    }

                    // The name entry owns the keys before the field edit
                    // does; the two are never both open (#283).
                    if self.profiles_ui.as_ref().is_some_and(|ui| ui.renaming.is_some()) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => self.profiles_cancel_rename(),
                            Key::Named(NamedKey::Enter) => self.profiles_commit_rename(),
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // A typed edit owns the keys before the list does.
                    if self.profiles_ui.as_ref().is_some_and(|ui| ui.editing.is_some()) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.editing = None;
                                }
                            }
                            // Enter is now one exit among several rather than
                            // the only one that commits (#272). An emptied
                            // string is the file's spelling of "unset" — it
                            // parses, writes, and resolution falls back
                            // through Defaults for it (profiles.rs's
                            // contract).
                            Key::Named(NamedKey::Enter) => {
                                self.profiles_commit_edit();
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            self.profiles_activate_selected();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.profiles_adjust(1);
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.profiles_adjust(-1);
                        }
                        Key::Named(NamedKey::Escape) => {
                            // Layered: edit and menu were handled above, so a
                            // filter clears first, and a second Esc CLOSES
                            // THE TAB — closing it is closing a tab (§12
                            // takes §11's rule whole).
                            let filtered = self
                                .profiles_ui
                                .as_ref()
                                .is_some_and(|ui| !ui.filter.is_empty());
                            if filtered {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.filter.clear();
                                    ui.selected = 0;
                                    ui.scroll_to_selected = true;
                                }
                            } else {
                                self.close_profiles_tab();
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    true,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    false,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // A full-pane screen owns the keyboard the way an overlay
                // does, minus the filter: Esc returns to the grid, chords
                // still work, and nothing falls through to the shell — the
                // user is not looking at it.
                if self.screen != AppScreen::Terminal {
                    use winit::keyboard::{Key, NamedKey};

                    // The fleet screen's code entry owns the keys while it is
                    // open — the settings tab's edit-buffer discipline. Esc
                    // drops the entry, not the screen; a second Esc leaves.
                    if self.screen == AppScreen::Fleet && self.enroll_entry.is_some() {
                        // Paste and typing land in the box the same way —
                        // same alphabet filter, same clamp. This box was the
                        // only one that ever took a paste (#228); #251 gave
                        // the other six the same path, and this one now
                        // shares it, minus the free insert: a code has an
                        // alphabet, so `push_code_chars` stays in the middle.
                        if let Some(cmd) = command_for(&event.logical_key, self.modifiers) {
                            let text = match &cmd {
                                TextCommand::Insert(s) => Some(s.clone()),
                                TextCommand::Paste => self.paste_text(&cmd),
                                _ => None,
                            };
                            if let Some(text) = text {
                                if let Some(edit) = self.enroll_entry.as_mut() {
                                    push_code_chars(edit, &text);
                                }
                                self.mark_chrome_dirty();
                                return;
                            }
                            // ⌫ and the caret keys are ordinary text editing.
                            if let Some(edit) = self.enroll_entry.as_mut() {
                                let out = edit.buffer.apply(cmd, None);
                                if out.changed {
                                    edit.error = false;
                                }
                                if let Some(copied) = out.copied {
                                    self.set_clipboard(copied);
                                }
                                self.mark_chrome_dirty();
                                return;
                            }
                        }
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                self.enroll_entry = None;
                            }
                            Key::Named(NamedKey::Enter) => {
                                let code = self
                                    .enroll_entry
                                    .as_ref()
                                    .map(|e| e.buffer.text().trim().to_string())
                                    .unwrap_or_default();
                                if code.is_empty() {
                                    // An empty Enter marks the box rather
                                    // than spending a request on it.
                                    if let Some(edit) = self.enroll_entry.as_mut() {
                                        edit.error = true;
                                    }
                                } else {
                                    self.enroll_entry = None;
                                    self.spawn_enroll(code);
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // While the browser hand-off waits, Esc cancels it before
                    // it leaves the screen — the code entry's layered-Esc
                    // discipline, applied to the other sign-in door.
                    if self.screen == AppScreen::Fleet
                        && matches!(self.account, AccountState::Linking { .. })
                        && matches!(&event.logical_key, Key::Named(NamedKey::Escape))
                    {
                        self.cancel_link();
                        return;
                    }

                    if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                        self.show_screen(AppScreen::Terminal);
                        return;
                    }
                    if let Some(binding) = keymap::lookup(&event.logical_key, event.physical_key, self.modifiers) {
                        self.perform(binding.action);
                    }
                    return;
                }

                // Every global chord resolves through the one binding table.
                // Adding a chord as an if-block here instead of a BINDINGS
                // row is the bug the command palette exists to prevent: the
                // palette renders and runs from BINDINGS, so an unlisted
                // chord is an undiscoverable one.
                if let Some(binding) = keymap::lookup(&event.logical_key, event.physical_key, self.modifiers) {
                    let swallow = match binding.when {
                        keymap::When::Always => true,
                        // In the alternate screen the chord falls through to
                        // the encoder rather than being swallowed: `less` and
                        // `vim` page themselves and are owed the bytes.
                        keymap::When::NotAltScreen => !self.tabs.active_source().is_some_and(|s| {
                            s.terminal().lock().modes().contains(zest_core::Modes::ALT_SCREEN)
                        }),
                    };
                    if swallow {
                        self.perform(binding.action);
                        return;
                    }
                }

                let Some(session) = self.tabs.active_source() else { return };
                let modes = session.terminal().lock().modes();

                if let Some(bytes) = key::encode(&event, self.modifiers, modes) {
                    // The guess is made from the key, never from the bytes,
                    // and before the write so it is on screen the same frame
                    // the keystroke leaves. Only a press: the release the
                    // kitty protocol encodes echoes nothing.
                    if event.state == ElementState::Pressed {
                        session.predict(
                            predict_key(&event.logical_key, self.modifiers),
                            self.predict_policy(),
                        );
                    }
                    // Written synchronously, before anything else. Deferring
                    // input to the next frame adds a whole frame of latency for
                    // nothing.
                    session.write(bytes);
                    let mut term = session.terminal().lock();
                    // Typing scrolls back to the bottom, which is what every
                    // terminal does and what users expect -- unless they asked
                    // it not to, which is the whole point of the setting.
                    if self.config.scroll_on_keypress {
                        term.scroll_to_bottom();
                    }
                    // ...and clears the selection, which is now stale. Not
                    // gated: a selection made before this keystroke is stale
                    // wherever the view happens to be sitting.
                    term.set_selection(None);
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_pos = (position.x, position.y);

                // A held slider follows the pointer before anything else —
                // including off the track, which is how every slider works.
                if let Some(row) = self.slider_drag {
                    self.apply_slider_at(row, position.x as f32);
                    return;
                }

                // A held font row reorders as it crosses its siblings: order
                // IS the setting, and each crossing writes through the same
                // path as every other edit (§11).
                if let Some((row, from)) = self.settings_ui.as_ref().and_then(|ui| ui.list_drag)
                {
                    if let Some(HitRegion::SettingsListItem(r, to)) =
                        self.chrome_hit(position.x, position.y)
                    {
                        if r == row && to != from {
                            self.reorder_list_item(row, from, to);
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.list_drag = Some((row, to));
                            }
                        }
                    }
                    return;
                }

                // The chrome sees the pointer first — unless a grid drag is in
                // progress, which keeps the grid: a selection that wanders
                // into the strip must not die there.
                if !self.mouse.is_dragging() {
                    let over = self.chrome_hit(position.x, position.y);
                    if over != self.chrome_hover {
                        self.chrome_hover = over;
                        self.mark_chrome_dirty();
                    }
                    // The resize edges are the one hit region with no visible
                    // affordance, so the cursor is the whole of it: without
                    // this the window is resizable and looks like it is not.
                    // Set only on change — this runs per mouse-move, and a
                    // Win32 call per move is not free.
                    let want = match over {
                        Some(HitRegion::Resize(edge)) => edge.into(),
                        _ => winit::window::CursorIcon::Default,
                    };
                    if want != self.cursor {
                        self.cursor = want;
                        if let Some(w) = self.window.as_ref() {
                            w.set_cursor(want);
                        }
                    }
                    if over.is_some() {
                        return;
                    }
                }

                let cell = self.cell_at(position.x, position.y);
                let moved = cell != self.pointer_cell;
                self.pointer_cell = cell;

                // Programs that enabled 1002 or 1003 want movement too -- that
                // is how a tmux pane drag or an htop hover works. Only on a cell
                // change: reporting every pixel would flood the pty.
                if moved && self.forward_motion(cell.0, cell.1) {
                    return;
                }

                if !self.mouse.is_dragging() {
                    return;
                }
                let Some(session) = self.tabs.active_source() else { return };
                let mut term = session.terminal().lock();
                if let (Some(mut sel), Some(pos)) =
                    (term.selection(), self.visual_abs_pos(&term, cell.0, cell.1))
                {
                    // Word mode extends by whole words, so dragging after a
                    // double-click grows the selection a word at a time rather
                    // than reverting to characters.
                    sel.head = if sel.mode == zest_core::SelectionMode::Word {
                        term.word_at(pos).1
                    } else {
                        pos
                    };
                    term.set_selection(Some(sel));
                    drop(term);
                    session.mark_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // A slider or list drag ends when any button releases,
                // wherever the pointer wandered to in the meantime.
                if state == ElementState::Released {
                    let slider = self.slider_drag.take().is_some();
                    let list = self
                        .settings_ui
                        .as_mut()
                        .and_then(|ui| ui.list_drag.take())
                        .is_some();
                    if slider || list {
                        return;
                    }
                }
                // Chrome clicks never reach the grid. A drag in progress keeps
                // the grid for symmetry with CursorMoved.
                if !self.mouse.is_dragging() {
                    if let Some(region) = self.chrome_hit(self.pointer_pos.0, self.pointer_pos.1) {
                        self.on_chrome_click(region, button, state);
                        return;
                    }
                }

                // A press in the grid proper is the user leaving the block
                // behind. Two selections lit at once — a block and a drag —
                // would be two answers to "what does ⌘⇧O copy".
                //
                // Here rather than inside the Left/Pressed arm below because
                // `active_source` borrows `self` and this needs `&mut self`.
                if button == MouseButton::Left && state == ElementState::Pressed {
                    self.set_selected_block(None);
                }

                let Some(session) = self.tabs.active_source() else { return };
                let (row, col) = self.pointer_cell;

                // When the program asked for mouse reporting, the mouse belongs
                // to it -- vim, htop, tmux and every TUI expect their clicks.
                // Selecting instead would make them appear broken.
                //
                // Shift is the escape hatch every terminal implements: hold it
                // to select text over a mouse-aware program anyway.
                if self.forward_mouse(button, state, row, col) {
                    return;
                }

                // The desktop chord plus a click copies *that* block's output,
                // wherever it is in scrollback -- which is the thing a keyboard
                // shortcut cannot express, since it can only mean "the last
                // one". No chrome and no hit map involved: a click in the grid
                // already resolves to a line, and a line already knows its
                // block.
                if button == MouseButton::Left
                    && state == ElementState::Pressed
                    && key::is_clipboard_chord(self.modifiers)
                {
                    self.copy_block_output_at(row);
                    return;
                }

                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        let mode = self.mouse.press(row, col);
                        let mut term = session.terminal().lock();
                        if let Some(pos) = self.visual_abs_pos(&term, row, col) {
                            let sel = select::begin(&term, pos, mode, self.modifiers.alt_key());
                            term.set_selection(Some(sel));
                        }
                        drop(term);
                        session.mark_dirty();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        self.mouse.release();
                        // Copy-on-select is deliberately NOT the default: it
                        // silently replaces the clipboard, which surprises people
                        // who selected only to read.
                        //
                        // PRIMARY is the exception, and the same argument is why:
                        // it *is* the selection, so writing it surprises nobody
                        // and clobbers nothing anyone copied. Middle-click reads
                        // it back, which is what makes the gesture below true.
                        #[cfg(all(unix, not(target_os = "macos")))]
                        {
                            let text = self
                                .tabs
                                .active_source()
                                .and_then(|s| s.terminal().lock().selection_text());
                            if let Some(text) = text.filter(|t| !t.is_empty()) {
                                self.set_primary(text);
                            }
                        }
                    }
                    // Middle-click pastes the selection, as X11 users expect --
                    // which for a long time it did not: it read CLIPBOARD, so it
                    // pasted whatever was last explicitly copied. PRIMARY is the
                    // selection; CLIPBOARD is the fallback where the session has
                    // no PRIMARY to offer.
                    (MouseButton::Middle, ElementState::Pressed) => self.paste_primary(),
                    (MouseButton::Right, ElementState::Pressed) => {
                        // Right-click copies when there is a selection and pastes
                        // otherwise -- the PowerShell/conhost convention Windows
                        // users already have in their fingers.
                        //
                        // Everything touching `session` happens first so its
                        // borrow ends before the clipboard calls, which need
                        // `&mut self`.
                        //
                        // A block's *body* is grid, not chrome, so this is
                        // also where right-clicking a block reaches its menu.
                        // The order matters and this is the whole of it:
                        // copy first, because the user selected text in order
                        // to copy it and stealing that for a menu would be
                        // the worst trade available.
                        let (text, block) = {
                            let mut term = session.terminal().lock();
                            let text = term.selection_text();
                            if text.is_some() {
                                term.set_selection(None);
                            }
                            // The block this row belongs to, if it has begun
                            // producing output. That predicate is exactly the
                            // one `build_block_views` draws headers on, so the
                            // menu opens on precisely the blocks that look
                            // like blocks — and the *live prompt*, which has
                            // no `output_line`, falls through to paste.
                            let block = self
                                .visual_line_at(&term, row)
                                .and_then(|line| term.blocks().block_at(line).cloned())
                                .filter(|b| b.output_line.is_some())
                                .map(|b| b.id.0);
                            (text, block)
                        };
                        match (text, block) {
                            (Some(t), _) => self.set_clipboard(t),
                            (None, Some(id)) => {
                                let at =
                                    [self.pointer_pos.0 as f32, self.pointer_pos.1 as f32, 0.0, 0.0];
                                self.open_block_menu(id, at);
                            }
                            (None, None) => self.paste(),
                        }
                    }
                    _ => {}
                }

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // The open launcher swallows the wheel without scrolling
                // anything: a menu that lets the grid scroll beneath it
                // reads as detached from the window it floats over. (It
                // grows its own scroll with the profiles editor, not here.)
                // The block menu joins it, and for a sharper reason: its
                // anchor is a *grid row*, so letting the grid scroll beneath
                // would slide the block out from under its own menu.
                if self.launcher.is_some() || self.block_menu.is_some() {
                    return;
                }
                // An open modal overlay takes the wheel wholesale. The
                // settings tab is below: not modal, so it scrolls only under
                // the pointer, by hit region, like the strip does.
                if self.picker.is_some() || self.palette_ui.is_some() {
                    let px = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y * 40.0,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32,
                    };
                    if px != 0.0 {
                        if let Some(p) = self.picker.as_mut() {
                            p.scroll -= px;
                        }
                        if let Some(p) = self.palette_ui.as_mut() {
                            p.scroll -= px;
                        }
                        self.mark_chrome_dirty();
                    }
                    return;
                }
                // Which surface owns this wheel is one classification over the
                // whole hit map (`hit::wheel_target`), not a list maintained
                // here. A list is what sent the scroll to the strip from a
                // block header, from the unfocused pane of a split, and from
                // any full-pane screen — a region nobody had classified fell
                // to the catch-all, and the terminal simply stopped scrolling.
                let hit = self.chrome_hit(self.pointer_pos.0, self.pointer_pos.1);
                let pane_focus = self.tabs.active().and_then(|t| t.is_split().then_some(t.focus));
                match hit::wheel_target(hit, pane_focus) {
                    WheelTarget::Swallow => return,
                    // A file scrolls by whole lines vertically and by pixels
                    // sideways — the same asymmetry the pane's own model
                    // keeps, because a line is the unit a reader thinks in and
                    // a column is not.
                    WheelTarget::Editor(i) => {
                        let rows = self
                            .tabs
                            .active()
                            .map_or(1, |t| self.editor_body_rows(t.pane_count()));
                        let (cell_w, body_w) = self.editor_body_span(i);
                        let (dx, dy) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x * cell_w * 3.0, y),
                            MouseScrollDelta::PixelDelta(p) => {
                                (p.x as f32, p.y as f32 / cell_w.max(1.0))
                            }
                        };
                        if let Some(e) =
                            self.tabs.active_mut().and_then(|t| t.pane_editor_mut(i))
                        {
                            e.scroll_by(dy, rows);
                            e.scroll_x_by(dx, cell_w, body_w);
                        }
                        self.mark_chrome_dirty();
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                        return;
                    }
                    // An open dropdown scrolls its *own* list, not the rows
                    // underneath: moving those would slide the anchor out
                    // from under it, and a 266-family roster has to be
                    // reachable by wheel (#259).
                    WheelTarget::Menu => {
                        let px = match delta {
                            MouseScrollDelta::LineDelta(_, y) => y * 40.0,
                            MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        };
                        for menu in [
                            self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut()),
                            self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut()),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            menu.scroll -= px;
                            // The wheel must not snap back to the selection —
                            // the `scroll_to_selected` rule every list keeps.
                            menu.scroll_to_selected = false;
                        }
                        self.mark_chrome_dirty();
                        return;
                    }
                    WheelTarget::Settings => {
                        let px = match delta {
                            MouseScrollDelta::LineDelta(_, y) => y * 40.0,
                            MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        };
                        if px != 0.0 {
                            // While the Profiles screen is up, these regions
                            // are its rows pane's (the Settings model is not
                            // built under a covering screen).
                            if self.profiles_tab_active() {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.scroll -= px;
                                }
                            } else if let Some(ui) = self.settings_ui.as_mut() {
                                ui.scroll -= px;
                            }
                            self.mark_chrome_dirty();
                        }
                        return;
                    }
                    WheelTarget::Strip => {
                        // Its own `px`, and the difference is load-bearing: a
                        // horizontal strip scrolls sideways, so the larger of
                        // the two axes wins here and only here.
                        let px = match delta {
                            MouseScrollDelta::LineDelta(x, y) => {
                                let step = if x.abs() > y.abs() { x } else { y };
                                step * 40.0
                            }
                            MouseScrollDelta::PixelDelta(p) => {
                                (if p.x.abs() > p.y.abs() { p.x } else { p.y }) as f32
                            }
                        };
                        if px != 0.0 {
                            // Layout clamps; storing the raw value would let the
                            // scroll wander past the content and take clicks with it.
                            self.strip_scroll -= px;
                            self.mark_chrome_dirty();
                        }
                        return;
                    }
                    WheelTarget::Grid => {}
                }

                let Some(session) = self.tabs.active_source() else { return };
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Trackpads report pixels. Convert with the cell height so
                    // the feel matches a wheel.
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.fonts.as_ref().map_or(20.0, |f| f.cell_metrics().cell_h as f32);
                        p.y as f32 / ch
                    }
                };
                self.scroll_accum += lines;
                let whole = self.scroll_accum.trunc();
                self.scroll_accum -= whole;
                if whole == 0.0 {
                    return;
                }

                // A mouse-aware program gets the wheel. And in the alternate
                // screen there is no scrollback to move through anyway -- `less`
                // and `man` expect the wheel as arrow keys, so scrolling our own
                // (empty) history would look like the wheel doing nothing.
                let alt = session.terminal().lock().modes().contains(zest_core::Modes::ALT_SCREEN);
                // One source for both branches below. They used to be two
                // literal `3`s, which is how the alt-screen translation and
                // the scrollback move could have ended up scrolling at
                // different speeds the first time either was touched.
                let rows = wheel_rows(whole, self.config.lines_per_notch);
                let count = rows.unsigned_abs();
                // Direction off the same value as the magnitude, not off
                // `whole`: one source means the arrow keys the alternate
                // screen gets and the scrollback move cannot disagree about
                // which way a gesture went, whatever the clamp did to it.
                let up = rows > 0;

                if self.forward_wheel(up, count) {
                    return;
                }
                if alt && !self.modifiers.shift_key() {
                    let key: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                    let mut out = Vec::with_capacity(key.len() * count);
                    for _ in 0..count {
                        out.extend_from_slice(key);
                    }
                    session.write(out);
                    return;
                }

                session.terminal().lock().scroll_display(rows);
                pull_history_at_top(session);
                // The grid has already moved. The spring carries only the
                // *visual* debt -- start it that many rows behind and let it
                // run back to zero -- so the session, the selection and every
                // hit test stay integral while the drawing catches up.
                //
                // Not in the alternate screen: `vim` and `less` scroll by
                // design and easing that fights the program, which is what the
                // setting's own doc comment says. `alt` was computed above for
                // the arrow-key translation and means the same thing here.
                if self.config.smooth_scroll && !alt && self.motion_allowed() {
                    // `nudge` rather than a fresh spring: a second notch
                    // mid-glide has to keep the velocity it already had, or
                    // scrolling fast reads as a series of restarts.
                    self.scroll_spring.nudge(rows as f32);
                    // One row of debt at most — see `Spring::clamp_to`. A notch
                    // is typically three rows, and notches accumulate, so
                    // without this the grid is drawn several rows from where it
                    // belongs and the renderer has one overscan row to cover it
                    // with.
                    self.scroll_spring.clamp_to(1.0);
                    self.scroll_spring.retarget(0.0);
                } else {
                    self.scroll_spring.snap_to(0.0);
                }
                session.mark_dirty();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // Damage gates the frame entirely. An idle terminal must use 0%
                // GPU -- that is a hard requirement, and it is what separates a
                // real terminal from a demo. The chrome has its own latch,
                // set only by discrete events, so the guarantee survives it.
                let grid_dirty = self.tabs.active_source().is_some_and(|s| s.take_dirty());
                if grid_dirty {
                    // Title changes arrive as ordinary output damage; noticing
                    // them here keeps the tab label and the OS titlebar honest
                    // without inventing a new event for it.
                    let title = self
                        .tabs
                        .active_source()
                        .map(|s| crate::chrome::model::terminal_name(&s.terminal().lock()))
                        .unwrap_or_default();
                    if title != self.window_title {
                        if let Some(w) = self.window.as_ref() {
                            w.set_title(if title.is_empty() { "zesterm" } else { &title });
                        }
                        self.window_title = title;
                        self.chrome_dirty = true;
                        self.chrome_layout = None;
                    }
                }
                // Re-scan once per frame at most, and only when the grid
                // actually moved: an idle terminal produces no `grid_dirty`, so
                // the 0%-idle damage guarantee above is untouched by having a
                // find bar open.
                if grid_dirty && self.find.is_some() {
                    self.run_find();
                    // After the scan, so a page that lands is searched on the
                    // frame it arrives and the next request goes out behind
                    // it -- one page in flight, one frame apart.
                    self.pump_find_history();
                    self.mark_chrome_dirty();
                }
                // Integrated once per frame, before the damage test decides
                // anything: a spring in flight *is* damage, and asking it after
                // the test would need a second reason to draw.
                let animating = self.step_motion();
                if grid_dirty || self.chrome_dirty || animating {
                    // Applied here rather than in the parser thread: the policy
                    // is about what the user is looking at, and the parser has
                    // no business knowing that. It also means a flood costs one
                    // snap per frame, not one per line.
                    // Not while the find bar is holding a hit: a build printing
                    // lines must not yank the view off the match somebody just
                    // stepped to, which reads as the search having lost its
                    // place. With no current hit there is nothing to protect
                    // and the setting behaves as it always did.
                    if grid_dirty
                        && self.config.scroll_on_output
                        && self.find_state.selected().is_none()
                    {
                        if let Some(session) = self.tabs.active_source() {
                            session.terminal().lock().scroll_to_bottom();
                        }
                    }
                    // `redraw` clears the chrome latch only when a frame is
                    // actually presented.
                    self.redraw();
                }
            }

            _ => {}
        }
    }

    pub(crate) fn on_new_events(&mut self, cause: winit::event::StartCause) {
        // The animation clock fired: one repaint, then `about_to_wait`
        // schedules the next tick — or nothing, if the animator's condition
        // cleared in between. That is the settle guarantee in one place.
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            self.chrome_dirty = true;
            if let Some(session) = self.tabs.active_source() {
                session.mark_dirty();
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Open a file picker a click only *recorded*.
    ///
    /// Outside every event's dispatch, which is the whole reason the click
    /// does not open it: the picker is modal and pumps its own loop, and
    /// running it inside a winit event handler re-enters the loop that is
    /// already running. See [`App::pending_pick`]. Per window, because each
    /// has its own settings screen and its own pending request.
    pub(crate) fn drain_pending_pick(&mut self) {
        if let Some(request) = self.pending_pick.take() {
            self.run_file_picker(request);
        }
    }

    /// When this window next needs the loop to wake — a due screenshot, or
    /// the animation clock's one deadline. The process merges every
    /// window's answer into the loop's single control flow.
    pub(crate) fn next_wake(&self, now: std::time::Instant) -> NextWake {
        let shot = self.screenshot_at.map(|at| at.saturating_duration_since(now));
        next_wake(shot, self.anim_deadline())
    }
}
