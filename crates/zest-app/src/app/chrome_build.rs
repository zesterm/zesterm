//! Building the chrome model, and routing a click back out of it.
//!
//! Moved out of `app/mod.rs` (#554). These two halves belong together and are
//! the reason this module exists rather than two: `refresh_chrome` decides what
//! the chrome shows and `on_chrome_click` decides what a hit region means, and
//! `chrome/mod.rs` already stakes the crate on them agreeing -- "a hit region
//! and the rectangle it was drawn as come from the same computation, so they
//! cannot drift apart". Separating them by file is the first step to separating
//! them in fact.
//!
//! Both are far too long -- 933 and 669 lines -- and `check-size` pins them
//! here at those numbers. Cutting them along the overlay boundaries `layout()`
//! already dispatches on is #554 phase 3, deliberately not this commit.

use super::*;

impl App {
    /// The chrome's current claim on the window edges.
    ///
    /// Recomputed on demand rather than cached: it is a handful of multiplies,
    /// and a cached copy is one more thing that can disagree with the settings
    /// and the scale factor after a reload or a monitor change.
    pub(super) fn insets(&self) -> Insets {
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        self.insets_at(scale)
    }

    /// [`Self::insets`] for callers that know the scale before the window is
    /// stored — the shell-spawn path in `resumed` sizes the grid this way.
    pub(super) fn insets_at(&self, scale: f32) -> Insets {
        let strip = self.strip_shown().then_some(crate::chrome::insets::StripClaim {
            position: self.config.tabs.position,
            strip_height: self.config.tabs.strip_height,
            sidebar_width: self.config.tabs.sidebar_width,
        });
        Insets::resolved(self.config.padding, scale, strip)
    }

    /// Whether the strip is drawn at all.
    ///
    /// `custom_chrome` forces it, and that is load-bearing rather than a
    /// preference: with `show_single_tab` off and one tab open, a borderless
    /// window would have no titlebar, no caption buttons and nothing to drag —
    /// an undecorated rectangle with no way to move, maximize or close it.
    fn strip_shown(&self) -> bool {
        self.config.chrome.draws_caption()
            || self.config.tabs.show_single_tab
            || self.tabs.len() > 1
    }

    pub(crate) fn mark_chrome_dirty(&mut self) {
        self.chrome_dirty = true;
        self.chrome_layout = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Re-run the chrome layout if anything invalidated it.
    ///
    /// Called from both the input path and the redraw, so a click is always
    /// tested against the same rectangles the frame drew — the "no drift"
    /// property the layout tests pin, extended to runtime.
    pub(super) fn refresh_chrome(&mut self) {
        if !self.strip_shown()
            && self.picker.is_none()
            && self.palette_ui.is_none()
            && self.dir_picker.is_none()
            && self.launcher.is_none()
            // Load-bearing: with one tab and no custom chrome `strip_shown` is
            // false, so without this the block menu lives in a layout that is
            // thrown away before it is ever built — the menu opens and nothing
            // appears, with nothing to see in any log.
            && self.block_menu.is_none()
            && self.open_file.is_none()
            // The same trap again (#519): the find bar is chrome, and with the
            // strip hidden it would be laid out into a cache thrown away before
            // it is drawn — the bar opens, nothing appears, nothing logs it.
            && self.find.is_none()
            // The same trap as the block menu, one surface along: a file pane
            // *is* chrome, so with the strip hidden its whole body would be
            // laid out and thrown away, and the pane would show nothing with
            // nothing in a log to say why.
            && !self.tabs.active().is_some_and(crate::tabs::Tab::has_editor_pane)
            && !self.tabs.settings_open()
            && self.screen == AppScreen::Terminal
        {
            self.chrome_layout = None;
            return;
        }
        // The layout cache and the frame-pending latch are separate on
        // purpose: the input path refreshes the layout between frames, and
        // must not eat the redraw the change asked for.
        if self.chrome_layout.is_some() {
            return;
        }
        // Built before the font borrow below: row construction reads the
        // fleet and the tabs, never the fonts.
        let anim = self.anim_phase();
        let caret_on = anim.caret_on;
        let early_geometry = self.window.as_ref().map(|w| {
            let scale = w.scale_factor() as f32;
            (scale, w.inner_size())
        });
        let picker_rows = self.picker.is_some().then(|| self.build_picker());
        let picker_model = picker_rows.map(|(rows, actions, hosts_searched)| {
            let state = self.picker.as_mut().expect("is_some gated the build");
            state.actions = actions;
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            // A filter edit can strand the selection on a group label; land
            // it on the nearest row Enter can actually run.
            if matches!(state.actions.get(state.selected), Some(PickerAction::None) | None) {
                if let Some(first) = state
                    .actions
                    .iter()
                    .position(|a| !matches!(a, PickerAction::None))
                {
                    state.selected = first;
                }
            }
            crate::chrome::model::PickerModel {
                rows,
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
                hosts_searched,
                caret_on,
            }
        });

        // The launcher's rows, rebuilt per pass like the picker's: profiles
        // can change under an open menu via the config watcher, and rows
        // and actions must come from one pass or a click runs the wrong row.
        let launcher_rows = self.launcher.is_some().then(|| {
            let fallback = self.shell_fallback();
            let active_profile = self
                .tabs
                .active()
                .and_then(|t| t.identity.as_ref())
                .map(|i| i.name.clone());
            crate::launcher::build_rows(
                &self.settings,
                &self.fleet_view,
                &fallback,
                active_profile.as_deref(),
                keymap::chord_for(keymap::Action::OpenProfiles),
            )
        });
        let launcher_model = launcher_rows.map(|(rows, actions)| {
            let state = self.launcher.as_mut().expect("is_some gated the build");
            state.actions = actions;
            // A reload can shrink the rows under the selection; land it on
            // the nearest actionable row rather than off the end or on the
            // divider.
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            if matches!(
                state.actions.get(state.selected),
                Some(crate::launcher::LauncherAction::None) | None
            ) {
                state.selected = crate::launcher::step(&state.actions, state.selected, true);
            }
            crate::chrome::model::LauncherModel {
                rows,
                selected: state.selected,
                anchor: state.anchor,
            }
        });

        // Rebuilt every pass, like the launcher's: output can arrive under an
        // open menu and turn "Copy output" from faint to live — or, on the
        // chip menu, a command finishing turns the cd rows live.
        let block_menu_rows = self.block_menu.as_ref().and_then(|m| {
            let tab = self.tabs.active()?;
            let folded = self
                .folded_blocks
                .get(&tab.focused_addr())
                .is_some_and(|f| f.contains(&m.block));
            let term = tab.focused_session()?.terminal();
            let term = term.lock();
            let block = term.blocks().get(zest_core::BlockId(m.block))?.clone();
            Some(crate::block_menu::build_rows(
                &term,
                &block,
                folded,
                &keymap::chord_for(keymap::Action::CopyBlockOutput),
                &keymap::chord_for(keymap::Action::RerunLastCommand),
            ))
        });
        let block_menu_model = block_menu_rows.map(|(rows, actions)| {
            let state = self.block_menu.as_mut().expect("is_some gated the build");
            state.actions = actions;
            // Output arriving can enable a row, and a fold can flip one; land
            // the selection on the nearest live row rather than off the end or
            // on something drawn faint.
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            if !crate::block_menu::is_actionable(&state.actions, state.selected) {
                state.selected = crate::block_menu::first_actionable(&state.actions);
            }
            crate::chrome::model::BlockMenuModel {
                rows,
                selected: state.selected,
                anchor: state.anchor,
            }
        });

        // Before the `&mut self` borrow below, since it reads the tab.
        let open_file_model = self.open_file_model();
        let find_model = self.find_model();
        let dir_picker_model = self.dir_picker.as_mut().map(|state| {
            // Rows and their answers in one pass: `..` first when there is a
            // parent, then the children the filter keeps. The parallel
            // `rows` list is what Enter and a click act on, so the two are
            // built together and cannot drift.
            let filter = state.filter.text().to_lowercase();
            let mut rows = Vec::new();
            let mut answers: Vec<Option<String>> = Vec::new();
            if state.parent.is_some() {
                rows.push("\u{2191}  .. (parent directory)".to_string());
                answers.push(None);
            }
            let base = std::path::Path::new(&state.path);
            for name in &state.dirs {
                if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                    continue;
                }
                rows.push(name.clone());
                answers.push(Some(base.join(name).to_string_lossy().into_owned()));
            }
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            state.rows = answers;
            crate::chrome::model::DirPickerModel {
                rows,
                has_parent: state.parent.is_some(),
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
                loading: state.loading,
                error: state.error.clone(),
                truncated: state.truncated,
            }
        });

        let palette_model = self.palette_ui.as_mut().map(|state| {
            let (rows, actions) = keymap::palette(state.filter.text());
            state.actions = actions;
            // A filter edit can strand the selection on a header, a
            // reference row, or past the end; land it on the nearest
            // runnable command instead.
            state.selected = keymap::nearest_runnable(&state.actions, state.selected);
            crate::chrome::model::PaletteModel {
                rows,
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
            }
        });

        // The Settings tab's screen, built only while it holds the grid area
        // (its state persists while it is a background chip). Inputs gathered
        // before the &mut borrow of the tab state below; the clone is a
        // handful of provenance entries, on an event-driven rebuild.
        // Not built while a full-pane screen covers it: the screen's opaque
        // ground would shadow every region positionally anyway, but
        // `settings_tracks` is keyed by row index, not position — the
        // Profiles screen shares the widget vocabulary, and two panes'
        // tracks under one key would send a slider drag to the wrong file.
        let settings_inputs = (self.settings_tab_active()
            && self.screen == AppScreen::Terminal)
            .then(|| {
            (
                serde_json::to_value(&self.settings).unwrap_or(serde_json::Value::Null),
                self.provenance.clone(),
                self.shared.restart_pending.borrow().clone(),
                self.settings_error.clone(),
                self.unknown_keys.clone(),
                // The rail's visible categories — computed outside the &mut
                // borrow, by the same helper the click handler resolves
                // `SettingsCategory(i)` with, so a click can never land on
                // a different list than was drawn.
                self.visible_categories(),
            )
        });
        // Taken before the &mut borrow below, and taken *once*: opening the
        // menu again on every rebuild would make it impossible to dismiss.
        let start_menu_key = self.start_menu_key.take();
        // Read before the closure: it borrows `self.settings_ui` mutably, and
        // the capability answer is about the surface, not the form.
        let caps = self.capabilities();
        let settings_model = self.settings_ui.as_mut().zip(settings_inputs).map(
            |(ui, (values, provenance, restart_pending, error, unknown_keys, visible_cats))| {
                use crate::settings_ui as sui;
                // The footer counts every category; the rail badges only the
                // visible ones (§11: the filter hides empty categories).
                let (_, total) =
                    sui::modified_counts(&ui.fields, &values, &sui::categories(&ui.fields));
                let (counts, _) = sui::modified_counts(&ui.fields, &values, &visible_cats);
                let visible: Vec<(String, usize)> =
                    visible_cats.into_iter().zip(counts).collect();
                if !visible.iter().any(|(g, _)| *g == ui.category) {
                    if let Some((first, _)) = visible.first() {
                        ui.category = first.clone();
                        ui.selected = 0;
                        ui.scroll = 0.0;
                    }
                }
                let selected_category = visible
                    .iter()
                    .position(|(g, _)| *g == ui.category)
                    .unwrap_or(0);

                let (rows, actions, empty) = if ui.category == sui::UNKNOWN_CATEGORY {
                    let (mut rows, mut actions) = sui::build_unknown_rows(
                        &unknown_keys,
                        &provenance,
                        ui.filter.text(),
                        &zest_config::schema::keys(),
                    );
                    let empty = rows.is_empty().then(|| {
                        if unknown_keys.is_empty() {
                            "Every key in your files is a setting this build knows.".to_string()
                        } else {
                            format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text())
                        }
                    });
                    if !rows.is_empty() {
                        // The §11 warn banner: a key from a newer version is
                        // indistinguishable from a typo, so these warn
                        // rather than fail — and the rest of the file
                        // applied normally.
                        let n = rows.len();
                        let text = format!(
                            "{n} key{} in your config {} not settings. Kept rather than \
                             discarded, and warned about rather than failed on: a key from \
                             a newer version is indistinguishable from a typo. The rest of \
                             the file applied normally.",
                            if n == 1 { "" } else { "s" },
                            if n == 1 { "is" } else { "are" },
                        );
                        rows.insert(0, crate::chrome::model::SettingsRowModel::Notice { text });
                        actions.insert(0, sui::RowAction::None);
                    }
                    (rows, actions, empty)
                } else {
                    let (rows, actions) = sui::build_category_rows(
                        &ui.fields,
                        &values,
                        &provenance,
                        &ui.category,
                        ui.filter.text(),
                        ui.editing.as_ref(),
                        &restart_pending,
                        error.as_deref(),
                        &ui.installed,
                        caps,
                    );
                    let empty = rows
                        .is_empty()
                        .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text()));
                    (rows, actions, empty)
                };
                ui.actions = actions;
                // A filter edit can strand the selection on a banner or past
                // the end; land it on the nearest real row instead.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // `--screen settings-menu`, now that the rows exist. Opened
                // through the same state Enter arms, so the flag can never
                // show something a user could not have reached.
                if let Some(key) = start_menu_key {
                    if let Some(row) = ui.actions.iter().position(|a| {
                        matches!(a, sui::RowAction::Field(i)
                            if ui.fields.get(*i).is_some_and(|f| f.key == key))
                    }) {
                        ui.selected = row;
                        let roster: Vec<String> = crate::themes::ids();
                        let current = zest_config::ui::value_at(&values, &key)
                            .and_then(serde_json::Value::as_str);
                        ui.menu = Some(MenuState::roster(row, roster, current));
                    }
                }

                let menu = ui.menu.as_ref().and_then(|menu| {
                    let field_idx = match ui.actions.get(menu.row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    let value = zest_config::ui::value_at(&values, &field.key);
                    // The font list's value is an array; its first entry is
                    // the face the atlas actually shapes with, so that is the
                    // one the ✓ belongs on.
                    let current = match value {
                        Some(serde_json::Value::Array(a)) => {
                            a.first().and_then(serde_json::Value::as_str)
                        }
                        other => other.and_then(serde_json::Value::as_str),
                    };
                    let footer = (field.widget == zest_config::ui::Widget::ThemePicker)
                        .then(|| BROWSE_THEMES.to_string());
                    menu_model(menu, &ui.actions, &ui.fields, current, footer)
                });

                let config_path = zest_config::paths::config_file()
                    .or_else(|| {
                        zest_config::paths::config_dir()
                            .map(|d| d.join(zest_config::paths::CONFIG_FILE))
                    })
                    .map(|p| crate::status::shorten_home(&p.display().to_string()))
                    .unwrap_or_default();

                crate::chrome::model::SettingsScreenModel {
                    categories: visible
                        .into_iter()
                        .map(|(label, modified)| {
                            crate::chrome::model::SettingsCategoryModel { label, modified }
                        })
                        .collect(),
                    selected_category,
                    heading: ui.category.clone(),
                    prefix: if ui.category == sui::UNKNOWN_CATEGORY {
                        "\u{2014}".to_string()
                    } else {
                        sui::category_prefix(&ui.fields, &ui.category)
                    },
                    lede: sui::category_lede(&ui.category).to_string(),
                    rows,
                    empty,
                    selected: ui.selected,
                    filter: ui.filter.text().to_string(),
                    filter_caret: caret_of(&ui.filter),
                    scroll: ui.scroll,
                    ensure_visible: ui.scroll_to_selected,
                    modified_total: total,
                    config_path,
                    menu,
                }
            },
        );

        // The Profiles editor's screen (§12), built only while its pane
        // holds the grid area — the Settings tab's exact discipline: inputs
        // gathered before the &mut borrow of the tab state.
        let profiles_inputs = self.profiles_tab_active().then(|| {
            let hosts: Vec<(String, bool, bool)> = self.shared.fleet.get()
                .map(|f| {
                    f.snapshot()
                        .into_iter()
                        .map(|h| {
                            let online = h.is_online();
                            (h.label, online, h.local)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let local_host = hosts
                .iter()
                .find(|(_, _, local)| *local)
                .map_or_else(|| "this machine".to_string(), |(label, ..)| label.clone());
            (
                serde_json::to_value(&self.settings).unwrap_or(serde_json::Value::Null),
                self.settings.clone(),
                self.shell_fallback(),
                local_host,
                hosts.into_iter().map(|(label, online, _)| (label, online)).collect::<Vec<_>>(),
                self.effective_theme().to_string(),
            )
        });
        let profiles_model = self.profiles_ui.as_mut().zip(profiles_inputs).map(
            |(ui, (window_values, settings, fallback_command, local_host, hosts, window_theme))| {
                use crate::profiles_ui as pui;
                use crate::settings_ui as sui;
                // A profile deleted or renamed under the editor falls back
                // to Defaults rather than editing a ghost.
                let names = pui::rail_names(&settings);
                if !names.contains(&ui.profile) {
                    ui.profile = zest_config::profiles::RESERVED_PROFILE.to_string();
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    // The buffer was typed for a profile that no longer
                    // exists, and `editing` is keyed by field index alone —
                    // left alive, the next Enter would write it into
                    // `[profiles.defaults]`, the parent every other profile
                    // inherits from. Losing the keystrokes beats writing them
                    // somewhere nobody asked for (#272).
                    ui.editing = None;
                    ui.renaming = None;
                    ui.rename_error = None;
                }
                let selected_rail = names.iter().position(|n| *n == ui.profile).unwrap_or(0);
                let is_defaults = selected_rail == 0;

                let root = crate::launcher::profiles_root(&settings);
                let resolved = zest_config::profiles::resolve_profile(&root, &ui.profile);
                let schemes = pui::scheme_swatches();
                let ctx = pui::ProfileRowContext {
                    window_values: &window_values,
                    window_theme: &window_theme,
                    fallback_command: &fallback_command,
                    local_host: &local_host,
                    hosts: &hosts,
                    schemes: &schemes,
                    is_defaults,
                };
                let (rows, chips, actions) = pui::build_profile_rows(
                    &ui.fields,
                    &resolved,
                    &ctx,
                    ui.filter.text(),
                    ui.editing.as_ref(),
                    ui.error.as_deref(),
                );
                ui.actions = actions;
                // A filter edit can strand the selection on a section rule
                // or past the end; land it on the nearest real row.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // The open dropdown, resolved same-pass against the row —
                // window.backdrop's variants, and §12's theme and font
                // rosters, through the Settings tab's own builder.
                let overrides = pui::overrides_json(&resolved);
                let menu = ui.menu.as_ref().and_then(|menu| {
                    let field_idx = match ui.actions.get(menu.row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    let value = pui::effective_value(field, &resolved, &overrides, &ctx);
                    let current = match &value {
                        serde_json::Value::Array(a) => {
                            a.first().and_then(serde_json::Value::as_str)
                        }
                        other => other.as_str(),
                    };
                    // The ✓ is matched against the *drawn* option, and the host
                    // field's on-disk spelling is not one: "no pin" is an empty
                    // string where the menu shows "(this machine)", and a pin
                    // written `Forge` is the row spelled `forge`. Without this
                    // fold the row every profile lands on is the one row that
                    // never shows a tick.
                    let folded;
                    let current = if field.widget == zest_config::ui::Widget::HostPicker {
                        folded = host_menu_selection(&menu.roster, current);
                        Some(folded.as_str())
                    } else {
                        current
                    };
                    let footer = (field.widget == zest_config::ui::Widget::ThemePicker)
                        .then(|| BROWSE_THEMES.to_string());
                    menu_model(menu, &ui.actions, &ui.fields, current, footer)
                });

                let display_name =
                    if is_defaults { "Defaults".to_string() } else { ui.profile.clone() };
                // Static, per the §12 caption's spirit: the real per-host
                // uname needs the control plane and belongs to the
                // cross-host launch item, not to a preview.
                let os_line =
                    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
                let preview =
                    pui::build_preview(&display_name, &resolved, &window_theme, &os_line);
                let count = pui::override_count(&ui.fields, &resolved);
                let empty = rows
                    .is_empty()
                    .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text()));

                let renaming = ui.renaming.as_ref().map(|buffer| {
                    crate::chrome::model::ProfileNameEdit {
                        buffer: buffer.text().to_string(),
                        caret: buffer.caret(),
                        selection: buffer.selection(),
                        error: ui.rename_error.clone(),
                    }
                });
                Box::new(crate::chrome::model::ProfilesScreenModel {
                    renaming,
                    rail: pui::build_rail(&settings, &fallback_command, &local_host),
                    selected_rail,
                    name: display_name,
                    command: resolved
                        .meta
                        .command
                        .clone()
                        .unwrap_or_else(|| fallback_command.clone()),
                    host_chip: resolved.meta.host.clone(),
                    icon: resolved.meta.icon.clone(),
                    accent: pui::accent_of(&resolved.meta),
                    can_delete: !is_defaults,
                    preview,
                    rows,
                    chips,
                    selected: ui.selected,
                    filter: ui.filter.text().to_string(),
                    filter_caret: caret_of(&ui.filter),
                    scroll: ui.scroll,
                    ensure_visible: ui.scroll_to_selected,
                    empty,
                    footer_sentence: pui::footer_sentence(is_defaults, count),
                    table_name: format!("[profiles.{}]", ui.profile),
                    menu,
                })
            },
        );

        // Built before the font borrow below: these read tabs, fleet and the
        // filesystem, never the fonts.
        let fleet_hosts = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();
        // The enrolled machine's account machinery, started on the daemon's
        // word instead of waiting for someone to open the Fleet screen —
        // see `should_start_account_watch`. Cheap per rebuild: a bool and a
        // scan of a handful of rows, and `account_poke` closes the gate the
        // moment the watch is up.
        if self.should_start_account_watch(&fleet_hosts) {
            if self.account == AccountState::Unknown {
                self.probe_account();
            }
            self.start_account_watch();
        }
        // Retained beside the model it feeds: the fleet screen's hit map
        // carries card indices, and they must resolve against the snapshot
        // the cards were built from, not a fresher one.
        self.fleet_view = fleet_hosts.clone();
        self.devices_view = self.shared.fleet.get().map(|f| f.devices()).unwrap_or_default();
        // Same retention rule for the theme gallery: card index i must mean
        // the same theme at click time that it meant at draw time.
        self.themes_view = crate::themes::ids();
        let screen_model = profiles_model
            .map(crate::chrome::model::ScreenModel::Profiles)
            .or_else(|| self.build_screen_model(&fleet_hosts));
        let panes = self.build_panes_model(&fleet_hosts);
        let grid_area = early_geometry.map_or([0.0; 4], |(scale, size)| {
            self.insets_at(scale).grid_rect(size.width, size.height)
        });

        // Before the font borrow below, like everything else the model reads.
        let notice = self.pairing_notice();
        let approval = self.approval_model();

        let Some(window) = self.window.as_ref() else { return };
        let Some(fonts) = self.fonts.as_mut() else { return };

        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let cm = fonts.cell_metrics();
        let metrics = ChromeMetrics {
            width: size.width as f32,
            height: size.height as f32,
            scale,
            strip_height: self.config.tabs.strip_height as f32,
            sidebar_width: self.config.tabs.sidebar_width as f32,
            line_height: cm.cell_h as f32,
            baseline: cm.baseline as f32,
            font_px: fonts.shaping_px(),
            cell_w: cm.cell_w as f32,
            padding: self.config.padding,
        };
        // In fullscreen the traffic lights auto-hide, so the strip reclaims
        // their reserve; everywhere else the answer comes from AppKit fresh,
        // because the inset is not a constant.
        //
        // Fullscreen also takes the caption buttons and the resize edges: the
        // OS owns the frame there, and drawing our own close button over a
        // fullscreen window would be offering to do something the window is
        // not currently able to do.
        let fullscreen = window.fullscreen().is_some();
        let controls = WindowControls {
            native_leading: (!fullscreen)
                .then(|| platform::native_control_inset(window))
                .flatten()
                .map(|(x, y)| [x as f32 * scale, y as f32 * scale]),
            drawn_caption: self.config.chrome.draws_caption() && !fullscreen,
            maximized: window.is_maximized(),
            // A maximized window that resized from its edge would un-maximize
            // under the pointer, which is not what the drag meant.
            resizable_edges: self.config.chrome.draws_caption()
                && !fullscreen
                && !window.is_maximized(),
        };

        let local_label = fleet_hosts
            .iter()
            .find(|h| h.local)
            .map_or_else(|| "local".to_string(), |h| h.label.clone());

        // Host accent slots: the local machine is always slot 0; remote hosts
        // take the next slots in first-seen strip order, so a host keeps its
        // colour for the life of the window.
        let mut remote_slots = HostSlots::new();

        let tab_models: Vec<TabModel> = self
            .tabs
            .iter()
            .map(|tab| {
                // A brief lock per tab per chrome rebuild — rebuilds are
                // event-driven, so this is microseconds, not a frame cost.
                let (title, cwd, running, progress) = {
                    let term = tab.source().terminal();
                    let term = term.lock();
                    let title = crate::chrome::model::terminal_label(&term);
                    // A remote terminal's cwd never crosses the wire directly;
                    // its blocks do, and each carries the cwd it ran in.
                    let cwd = if term.cwd().is_empty() {
                        term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                    } else {
                        term.cwd().to_string()
                    };
                    let running = term.blocks().last().is_some_and(|b| b.is_running());
                    (title, cwd, running, term.progress())
                };
                let origin = match tab.source().origin() {
                    Origin::Daemon { host, local: false } => {
                        // The id is the tab's own address's — all-zero while a
                        // launch is still connecting, which is exactly what
                        // the variant's placeholder fallback is for (#304).
                        TabOrigin::Remote { host: tab.addr.host, label: host }
                    }
                    _ => TabOrigin::Local,
                };
                let (host_label, accent, cwd) = match &origin {
                    TabOrigin::Remote { label, .. } => {
                        let slot = remote_slots.slot(tab.addr, label);
                        (label.clone(), slot + 1, cwd)
                    }
                    TabOrigin::Local => {
                        (local_label.clone(), 0, crate::status::shorten_home(&cwd))
                    }
                };
                let age = self
                    .activity
                    .lock()
                    .get(&tab.addr)
                    .map(|t| crate::status::age_label(t.elapsed()))
                    .unwrap_or_default();
                // How this tab's host is reached — the fact the chip's glyph
                // tile inks when it degrades (the status bar's old job). A
                // dropped daemon link outranks whatever path the host
                // normally takes.
                let link = if self.link_down {
                    crate::chrome::model::LinkKind::Reconnecting
                } else if matches!(origin, TabOrigin::Local) {
                    crate::chrome::model::LinkKind::Loopback
                } else {
                    // The same lookup `presence` uses. These were two, matching
                    // differently, so with duplicate labels a tab could report
                    // one machine's presence beside another's route (#297).
                    let host = fleet_host_of(&origin, &fleet_hosts);
                    match host.and_then(|h| h.reachability) {
                        Some(zest_mesh::Reachability::Cloud) => {
                            crate::chrome::model::LinkKind::Tunnel
                        }
                        Some(zest_mesh::Reachability::Loopback) => {
                            crate::chrome::model::LinkKind::Loopback
                        }
                        _ => crate::chrome::model::LinkKind::Lan,
                    }
                };
                TabModel {
                    addr: tab.addr,
                    kind: crate::chrome::model::TabKind::Session,
                    title: if tab.dead { format!("{title} · ended") } else { title },
                    host: host_label,
                    cwd,
                    // The fleet's word about this tab's machine (#297). Three
                    // of `TabPresence`'s four variants had never been produced
                    // for a tab, so a session on a machine whose port had
                    // stopped answering read exactly like one on a healthy
                    // machine — the chip's "· unreachable" was drawn by code
                    // nothing could reach.
                    presence: tab_presence(&origin, &fleet_hosts),
                    origin,
                    accent,
                    tab_accent: crate::chrome::model::tab_accent(tab.identity.as_ref(), accent),
                    running,
                    progress,
                    attention: self.attention.get(&tab.addr).copied(),
                    age,
                    // Dead tabs borrow the connecting style (faint text): not
                    // live, not interactive, still present. A launching tab
                    // wears it for real (issue #175).
                    connecting: tab.dead || tab.connecting,
                    link,
                    opacity: tab.identity.as_ref().and_then(|i| i.opacity),
                }
            })
            .collect();

        // App tabs after the session tabs, in §1's order: sessions, then
        // Profiles, then Settings, then the `+`. One list, so the strip, the
        // sidebar rows and the hit map all agree what exists — including in
        // the vertical position, which Profiles used to be gated out of
        // entirely: ⌘⇧, opened a pane the sidebar could neither show nor
        // close (#494).
        let mut tab_models = tab_models;
        if self.tabs.profiles_open() {
            tab_models.push(TabModel {
                addr: crate::tabs::profiles_tab_addr(),
                kind: crate::chrome::model::TabKind::Profiles,
                title: "Profiles".into(),
                // Empty, like Settings': an app tab is a place, not a shell
                // on a host, and the vertical header draws its host pill off
                // exactly this field. The local label sat here harmlessly
                // while Profiles was horizontal-only — the horizontal strip
                // has no header — so making the tab appear in both positions
                // is what turned it into a visible "Profiles · local".
                host: String::new(),
                cwd: String::new(),
                origin: TabOrigin::Local,
                presence: TabPresence::Online,
                accent: 0,
                // Accent index 0 is the theme's own accent: an app tab is a
                // place, not a shell on a host.
                tab_accent: crate::chrome::model::AccentChoice::Profile(0),
                running: false,
                progress: zest_core::Progress::None,
                attention: None,
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
                // An app tab is a place, not a shell on a host: no pane, so
                // nothing to match.
                opacity: None,
            });
        }
        if self.tabs.settings_open() {
            tab_models.push(TabModel {
                addr: crate::tabs::settings_addr(),
                kind: crate::chrome::model::TabKind::Settings,
                title: "Settings".to_string(),
                host: String::new(),
                cwd: String::new(),
                origin: TabOrigin::Local,
                presence: TabPresence::Online,
                accent: 0,
                tab_accent: crate::chrome::model::tab_accent(None, 0),
                running: false,
                progress: zest_core::Progress::None,
                attention: None,
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
                // An app tab is a place, not a shell on a host: no pane, so
                // nothing to match.
                opacity: None,
            });
        }

        // The sidebar's host grouping, built from the same tab models the
        // strip draws — one pass, one truth. App tabs are places with no
        // host; the vertical layout pins them above the footer instead.
        let sidebar = (self.config.tabs.position == zest_config::settings::TabsPosition::Left)
            .then(|| {
                let mut groups: Vec<crate::chrome::model::HostGroup> = Vec::new();
                for (i, tm) in tab_models.iter().enumerate() {
                    if tm.kind != crate::chrome::model::TabKind::Session {
                        continue;
                    }
                    if let Some(g) = groups.iter_mut().find(|g| g.label == tm.host) {
                        g.tabs.push(i);
                        continue;
                    }
                    let fleet = fleet_hosts.iter().find(|h| h.label == tm.host);
                    let sub = match fleet.and_then(|h| h.reachability) {
                        Some(zest_mesh::Reachability::Loopback) => "loopback".to_string(),
                        Some(zest_mesh::Reachability::Lan) => match fleet.and_then(|h| h.rtt_ms) {
                            Some(ms) => format!("LAN {}", crate::chrome::layout::format_ms(ms)),
                            None => "LAN".to_string(),
                        },
                        Some(zest_mesh::Reachability::Cloud) => match fleet.and_then(|h| h.rtt_ms) {
                            Some(ms) => format!("tunnel {}", crate::chrome::layout::format_ms(ms)),
                            None => "tunnel".to_string(),
                        },
                        None => String::new(),
                    };
                    groups.push(crate::chrome::model::HostGroup {
                        label: tm.host.clone(),
                        accent: tm.accent,
                        sub,
                        online: fleet.is_none_or(crate::fleet::FleetHost::is_online),
                        tabs: vec![i],
                    });
                }
                // The footer says "N hosts online · M asleep", and until #237 a
                // relay-reachable machine was counted in M — the same wrong
                // answer the card gave, in a second place.
                let online = fleet_hosts.iter().filter(|h| h.is_online()).count().max(1);
                let asleep = fleet_hosts.len().saturating_sub(online);
                crate::chrome::model::SidebarModel {
                    groups,
                    hosts_online: online,
                    hosts_asleep: asleep,
                }
            });

        // Ungated from `TabsPosition::Left` (#385). `running` has been computed
        // for every tab all along and read at exactly one site — the sidebar's
        // dot — so the horizontal strip showed nothing at all while a command
        // ran. The clock is what the chip's ring and the row's dot both turn
        // on, and neither position has a claim on it.
        self.anim_pulse = tab_models.iter().any(|t| t.running);
        // Only what actually *turns*. A determinate arc is a static picture
        // that changes when the number does, so keeping the 80ms timer alive
        // for it would spend a frame every 80ms redrawing an identical ring —
        // and 0%-idle is a property this app has tests for, not a hope.
        self.anim_spin_tabs = tab_models.iter().any(|t| {
            matches!(t.progress, zest_core::Progress::Indeterminate)
                || (t.running && !t.progress.is_busy())
        });

        // Which chip is lit — exactly one (invariant 9), and the strip is
        // what says so. This used to be derived here instead, because
        // `display_active` knew only about Settings; both now walk the same
        // drawn order (sessions, Profiles, Settings) so the chrome and the
        // ⌘⇧] cycle cannot disagree about which tab is which.
        let active = self.tabs.display_active();

        let model = ChromeModel {
            tabs: tab_models,
            active,
            position: self.config.tabs.position,
            strip_scroll: self.strip_scroll,
            ensure_active_visible: self.strip_ensure_visible,
            hover: self.chrome_hover,
            controls,
            focused: self.focused,
            sidebar,
            screen: screen_model,
            panes,
            grid_area,
            anim,
            palette_chord: keymap::chord_for(keymap::Action::ToggleFleetPicker),
            settings_chord: keymap::chord_for(keymap::Action::ToggleSettings),
            profiles_chord: keymap::chord_for(keymap::Action::OpenProfiles),
            picker: picker_model,
            // The picker wins: it opens *over* the settings tab's content.
            palette: palette_model,
            dir_picker: dir_picker_model,
            open_file: open_file_model,
            find: find_model,
            settings: settings_model,
            launcher: launcher_model,
            block_menu: block_menu_model,
            // The pairing line wins a collision: it is about something that
            // happened to this machine and stays until it is dealt with, where
            // a drop hint is about a gesture in flight and comes back the next
            // time a file crosses the window.
            notice: notice.or_else(|| self.drop_hint.clone()),
            approval,
            confirm_close: self.confirm_close.clone(),
        };

        let colors = self.chrome_colors;
        let mut measure = |s: &str, px: f32, bold: bool, tracking: f32| {
            zest_render_wgpu::measure_ui_run(fonts, s, zest_font::Style::new(bold, false), px, tracking)
        };
        let laid = crate::chrome::layout::layout(&model, &colors, &metrics, &mut measure);
        self.strip_scroll = laid.strip_scroll;
        // One layout consumed the request; the wheel is free again — the
        // same discipline as the overlays' scroll_to_selected below.
        self.strip_ensure_visible = false;
        if let Some(state) = self.picker.as_mut() {
            state.scroll = laid.picker_scroll;
            // One layout consumed the request; the wheel is free again.
            state.scroll_to_selected = false;
        }
        if let Some(state) = self.palette_ui.as_mut() {
            state.scroll = laid.palette_scroll;
            // One layout consumed the request; the wheel is free again.
            state.scroll_to_selected = false;
        }
        if let Some(state) = self.dir_picker.as_mut() {
            state.scroll = laid.dir_picker_scroll;
            state.scroll_to_selected = false;
        }
        // Written back only when the pane actually laid out — a covered tab
        // reports 0.0, and writing that would reset a scroll the user set.
        if self.tabs.settings_active() && self.screen == AppScreen::Terminal {
            if let Some(state) = self.settings_ui.as_mut() {
                state.scroll = laid.settings_scroll;
                // One layout consumed the request; the wheel is free again.
                state.scroll_to_selected = false;
            }
        }
        if self.profiles_tab_active() {
            if let Some(state) = self.profiles_ui.as_mut() {
                state.scroll = laid.profiles_scroll;
                // One layout consumed the request; the wheel is free again.
                state.scroll_to_selected = false;
            }
        }
        self.chrome_layout = Some(laid);
    }

    /// What the pointer is over in the chrome, using the current layout.
    pub(super) fn chrome_hit(&mut self, x: f64, y: f64) -> Option<HitRegion> {
        self.refresh_chrome();
        // Cached chrome first — its overlays and scrims must outrank the
        // per-frame block headers, whose regions only exist inside the grid
        // area anyway.
        self.chrome_layout
            .as_ref()
            .and_then(|l| l.hit.hit(x as f32, y as f32))
            .or_else(|| self.chip_hits.hit(x as f32, y as f32))
            .or_else(|| self.block_hits.hit(x as f32, y as f32))
    }

    /// A pointer action that landed in the chrome.
    pub(super) fn on_chrome_click(
        &mut self,
        region: HitRegion,
        button: MouseButton,
        state: ElementState,
    ) {
        if state != ElementState::Pressed {
            return;
        }
        // An open dropdown menu closes on any settings click that is not one
        // of its own rows — choosing elsewhere means "never mind".
        if self.settings_ui.as_ref().is_some_and(|ui| ui.menu.is_some())
            && !matches!(region, HitRegion::SettingsMenuRow(_))
        {
            if let Some(ui) = self.settings_ui.as_mut() {
                ui.menu = None;
            }
            self.mark_chrome_dirty();
        }
        if self.profiles_ui.as_ref().is_some_and(|ui| ui.menu.is_some())
            && !matches!(region, HitRegion::SettingsMenuRow(_))
        {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.menu = None;
            }
            self.mark_chrome_dirty();
        }
        // A click anywhere else in either editor is leaving the open field, so
        // it commits first (#272, #275) — and stays put when the buffer cannot
        // be written, which is what stops a stray click destroying it.
        //
        // One guard here rather than one per arm: the per-arm version is what
        // let `Enter` become the only exit that wrote in the first place, and
        // an arm added later inherits this instead of having to remember it.
        // Menu regions are excluded on purpose — a dropdown is modal and owns
        // its own keys, so a click inside it is not leaving anything.
        let in_menu = matches!(
            region,
            HitRegion::SettingsMenuRow(_)
                | HitRegion::SettingsMenuSearch
                | HitRegion::SettingsMenuPanel
                | HitRegion::SettingsMenuFooter
        );
        if !in_menu {
            if self.settings_tab_active() && !self.settings_commit_edit() {
                return;
            }
            // The name entry is its own control: clicking it is not leaving
            // it, and committing here would close what the click is opening.
            let on_name = region == HitRegion::ProfilesName;
            if self.profiles_tab_active() && !on_name && !self.profiles_commit_edit() {
                return;
            }
        }
        match (region, button) {
            (HitRegion::ApprovalApprove, MouseButton::Left) => self.decide_approval(true),
            (HitRegion::ApprovalDeny, MouseButton::Left) => self.decide_approval(false),
            (HitRegion::ConfirmClose, MouseButton::Left) => {
                self.answer_confirm_close(Some(true));
            }
            (HitRegion::ConfirmDetach, MouseButton::Left) => {
                self.answer_confirm_close(Some(false));
            }
            (HitRegion::ConfirmCancel, MouseButton::Left) => {
                self.answer_confirm_close(None);
            }
            // The panel and its scrim swallow, and deliberately do not
            // dismiss: one of the three answers destroys a running command,
            // and "clicked it away" must not be able to reach any of them.
            (HitRegion::ConfirmPanel, _) => {}
            // The panel (and its full-window scrim) swallows everything
            // else: a security prompt neither dismisses on a stray click
            // nor lets one fall through to the grid beneath it.
            (HitRegion::ApprovalPanel, _) => {}
            (HitRegion::Tab(addr), MouseButton::Left) => {
                // The Profiles chip is an app tab, not a session: clicking
                // it shows its pane. Through the open path (idempotent) so
                // the editor state is guaranteed to exist behind the screen.
                if addr == crate::tabs::profiles_tab_addr() {
                    self.open_profiles_tab();
                    return;
                }
                // Even when it is already the active one: clicking a session
                // means "show me this", which no screen may overrule.
                self.leave_screen();
                if self.tabs.activate_addr(addr) {
                    self.after_activation();
                }
            }
            (HitRegion::TabClose(addr), MouseButton::Left)
            | (HitRegion::Tab(addr), MouseButton::Middle) => {
                if addr == crate::tabs::profiles_tab_addr() {
                    // No chip × exists (app tabs carry none), but middle
                    // click closes every other tab and must not skip this
                    // one — closing it is closing a tab.
                    self.close_profiles_tab();
                    return;
                }
                self.close_tab(addr, false);
            }
            (HitRegion::NewTab, MouseButton::Left) => {
                // The + opens the launcher menu (design §1) — there is no
                // separate default-only half; ⌘T still spawns the default
                // directly, so it stays one keystroke away.
                self.toggle_launcher();
            }
            (HitRegion::LauncherRow(i), MouseButton::Left) => {
                if let Some(l) = self.launcher.as_mut() {
                    l.selected = i;
                }
                let action = self.launcher.as_ref().and_then(|l| l.actions.get(i).cloned());
                if let Some(action) = action {
                    self.run_launcher_action(action);
                } else {
                    self.mark_chrome_dirty();
                }
            }
            // A click on the panel beside a row chose nothing; swallowing it
            // is what makes a near-miss not a dismissal.
            (HitRegion::LauncherPanel, _) => {}
            (HitRegion::LauncherScrim, MouseButton::Left) => {
                self.launcher = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::FindPrev, MouseButton::Left) => self.step_find(-1),
            (HitRegion::FindNext, MouseButton::Left) => self.step_find(1),
            (HitRegion::FindClose, MouseButton::Left) => self.close_find(),
            (HitRegion::FindCase, MouseButton::Left) => {
                // Smart case reads the query, so forcing it here would be a
                // second source of truth for the same fact. Typing a capital
                // is the toggle; the chip reports it.
                self.run_find();
                self.mark_chrome_dirty();
            }
            // Swallowed: a click beside the entry must not reach the grid and
            // move the selection out from under the search.
            (HitRegion::FindPanel, _) => {}
            (HitRegion::PalettePill, MouseButton::Left)
            | (HitRegion::SidebarSearch, MouseButton::Left) => {
                self.perform(keymap::Action::ToggleFleetPicker);
            }
            (HitRegion::FleetFooter, MouseButton::Left) => {
                // A toggle: the button that opened the fleet view is the most
                // discoverable way back out of it.
                self.show_screen(if self.screen == AppScreen::Fleet {
                    AppScreen::Terminal
                } else {
                    AppScreen::Fleet
                });
            }
            // The screen's ground swallows; its cards claim their own.
            (HitRegion::ScreenPanel, _) => {}
            (HitRegion::Pane(i), MouseButton::Left)
            // Clicking into a file moves the keyboard there too, so a wheel
            // and a keystroke agree about which pane you are in.
            | (HitRegion::EditorBody(i), MouseButton::Left) => {
                if self.tabs.active_mut().is_some_and(|t| t.focus_pane(i)) {
                    self.mark_chrome_dirty();
                }
            }
            (HitRegion::ThemeCard(i), MouseButton::Left) => {
                // The retained snapshot, not a fresh `themes::ids()`: the
                // roster can change between the frame that drew the cards
                // and the click (an import, a config reload), and index i
                // must keep meaning the card the user aimed at.
                let id = self.themes_view.get(i).cloned();
                if let Some(id) = id {
                    self.apply_theme_choice(&id);
                }
            }
            (HitRegion::ThemeImport, MouseButton::Left) => {
                // The card's promise: whatever scheme file the clipboard
                // holds becomes a theme. Parse failures land on the card
                // itself — a click with feedback nowhere reads as dead UI,
                // which is exactly what this card spent its life as.
                let outcome = match self.shared.clipboard.borrow_mut().as_mut() {
                    // No OS clipboard connection at all is a different fact
                    // from an empty one — the user cannot fix it by copying.
                    None => Err("the clipboard is unavailable in this session".to_string()),
                    Some(clipboard) => match clipboard.get_text() {
                        Ok(text) if !text.trim().is_empty() => {
                            crate::themes::import_pasted(&text)
                        }
                        // Empty and non-text (an image, a file) both land
                        // here — arboard reports each as "not available",
                        // and the remedy is the same either way.
                        _ => Err("the clipboard has no text — copy a scheme file first"
                            .to_string()),
                    },
                };
                match outcome {
                    Ok(theme) => {
                        self.theme_import_error = None;
                        // Applying is the success feedback: the new card
                        // appears, ringed active.
                        self.apply_theme_choice(&theme.id);
                        // Directly too: a re-import of the *active* theme
                        // changes no config value, so the reload above
                        // classifies it as a no-op and would repaint nothing.
                        self.apply_theme();
                    }
                    Err(e) => {
                        self.theme_import_error = Some(e);
                        self.mark_chrome_dirty();
                    }
                }
            }
            (HitRegion::FleetCard(i), MouseButton::Left) => {
                // The card's promise is the picker row's: a fresh shell on
                // that machine. Routed against the retained snapshot the
                // card indices were built from, and through the exact
                // PickerAction::Create path — back to the grid, remote
                // creates pinned to the id the roster named.
                let target = self.fleet_view.get(i).map(|h| (h.host, self.best_route(h)));
                if let Some((host, Some(route))) = target {
                    let expect = (!route.is_local()).then_some(host);
                    self.screen = AppScreen::Terminal;
                    self.spawn_tab_worker_pinned(route, None, expect, true, None);
                    self.mark_chrome_dirty();
                }
                // No route: the card drew without a hit region, so this arm
                // is only reachable by a click racing a snapshot change —
                // ignoring it is the honest answer.
            }
            (HitRegion::FleetSession(i, j), MouseButton::Left) => {
                // Attach to what is already running there (#287) — the ⌘K
                // picker's `Attach` arm, reached from the screen that shows
                // you the fleet. Resolved against the retained snapshot the
                // card indices were built from, exactly as the card above is.
                //
                // The listing is re-read rather than trusted from the drawn
                // row, because the card is redrawn on every fleet change and
                // a click can land a frame behind one: an index into a stale
                // list would attach to a neighbouring session.
                let target = self.fleet_view.get(i).and_then(|h| {
                    let crate::fleet::SessionsState::Fresh(sessions) = &h.sessions else { return None };
                    let info = sessions.get(j)?;
                    Some((info.addr, self.best_route(h)))
                });
                let Some((addr, route)) = target else { return };
                // Already open here: activate that tab rather than opening a
                // second view of one session — the picker's rule, and the one
                // that keeps a fleet card from quietly duplicating tabs.
                // `after_activation` steps off a full-pane screen by itself.
                let acted = if self.tabs.activate_addr(addr) {
                    self.after_activation();
                    true
                } else if let Some(route) = route {
                    self.screen = AppScreen::Terminal;
                    self.spawn_tab_worker(route, Some(addr));
                    true
                } else {
                    false
                };
                // Leaving the fleet screen is part of *acting*, never a
                // consolation. A host can lose its route between the layout
                // pass that drew this row and the click that lands on it, and
                // dropping the user back to the terminal with nothing opened
                // would take away the view they had and give nothing for it —
                // the card arm above already refuses on the same grounds.
                if acted {
                    self.mark_chrome_dirty();
                }
            }
            (HitRegion::FleetEnrollLocal, MouseButton::Left) => {
                self.enroll_local_daemon();
            }
            (HitRegion::FleetApproveDevice(i), MouseButton::Left) => {
                // Approve or vouch — which verb is the row's state, decided
                // where the row was built (`fleet_device_rows`), against the
                // same snapshot this index resolves into.
                self.spawn_approve(i);
            }
            (HitRegion::FleetLinkStart, MouseButton::Left) => {
                self.spawn_link();
            }
            (HitRegion::FleetLinkCancel, MouseButton::Left) => {
                self.cancel_link();
            }
            (HitRegion::FleetSignIn, MouseButton::Left) => {
                // A fresh, empty entry: the keyboard owns it from here
                // (Enter enrols, Esc drops it). field_idx/append are the
                // settings tab's concerns and idle here.
                self.enroll_entry = Some(crate::settings_ui::EditBuffer {
                    field_idx: 0,
                    buffer: TextField::default(),
                    error: false,
                    list: crate::settings_ui::ListEdit::Value,
                });
                self.mark_chrome_dirty();
            }
            (HitRegion::FleetSignOut, MouseButton::Left) => {
                self.spawn_sign_out();
            }
            (HitRegion::BlockFold(id), MouseButton::Left) => {
                // Folding a block is acting on it, so it becomes the target
                // the chords and the menu mean.
                self.set_selected_block(Some(id));
                self.toggle_fold(id);
            }
            // The rail and the band are one target with two shapes: a click on
            // either selects the block.
            (HitRegion::BlockRail(id) | HitRegion::BlockHeader(id), MouseButton::Left) => {
                self.set_selected_block(Some(id));
            }
            (HitRegion::BlockMenu(id), MouseButton::Left) => {
                // Off the ⋯ the block pass drew, so the panel hangs from the
                // affordance that opened it. The pointer is the fallback for
                // the frame after a scroll moved the header.
                let anchor = self
                    .block_menu_anchor
                    .filter(|(b, _)| *b == id)
                    .map(|(_, r)| r)
                    .unwrap_or([self.pointer_pos.0 as f32, self.pointer_pos.1 as f32, 0.0, 0.0]);
                self.open_block_menu(id, anchor);
            }
            // Right-click on a block's chrome is the menu's other door.
            (
                HitRegion::BlockRail(id)
                | HitRegion::BlockHeader(id)
                | HitRegion::BlockFold(id)
                | HitRegion::BlockMenu(id),
                MouseButton::Right,
            ) => {
                let at = [self.pointer_pos.0 as f32, self.pointer_pos.1 as f32, 0.0, 0.0];
                self.open_block_menu(id, at);
            }
            // A chip click acts on what it shows, re-read from the view this
            // frame drew, never from the region — one computation. Most kinds
            // copy their value (silently, like the block menu's copy rows);
            // the two that can do better, do: the cwd chip opens its
            // recent-directories menu, and the exit chip selects its failed
            // block *and scrolls it into view* — there is nothing to copy
            // about a failure, there is somewhere to look.
            (HitRegion::PromptChip(kind), MouseButton::Left) => {
                use crate::chrome::prompt_chips::ChipKind;
                let chip = self
                    .prompt_chips_view
                    .as_ref()
                    .and_then(|v| v.chips.iter().find(|c| c.kind == kind))
                    .cloned();
                if let Some(chip) = chip {
                    match kind {
                        ChipKind::Cwd => {
                            self.open_dir_picker(chip.value);
                        }
                        ChipKind::Exit => {
                            if let Ok(id) = chip.value.parse::<u32>() {
                                self.set_selected_block(Some(id));
                                if let Some(session) = self.tabs.active_source() {
                                    let mut term = session.terminal().lock();
                                    if let Some(line) = term
                                        .blocks()
                                        .get(zest_core::BlockId(id))
                                        .map(|b| b.prompt_line)
                                    {
                                        term.scroll_to_line(line);
                                    }
                                }
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                            }
                        }
                        _ => self.set_clipboard(chip.value),
                    }
                }
            }
            // Anything else on a chip swallows, like the block chrome below:
            // the pill paints over prompt rows, and that text must not be
            // selectable through it.
            (HitRegion::PromptChip(_), _) => {}
            // Anything else on the block's chrome swallows: the band paints
            // over the prompt rows, and that text must not be selectable
            // through it — which is the reason it has a hit region at all.
            (
                HitRegion::BlockHeader(_) | HitRegion::BlockRail(_) | HitRegion::BlockMenu(_),
                _,
            ) => {}
            (HitRegion::BlockMenuRow(i), MouseButton::Left) => {
                let action = self.block_menu.as_ref().and_then(|m| m.actions.get(i).copied());
                if let Some(action) = action {
                    self.run_block_menu_action(action);
                }
            }
            // A near-miss beside a row chose nothing; swallowing is what makes
            // it not a dismissal.
            (HitRegion::BlockMenuPanel, _) => {}
            // Any button, not just Left: a right-click away from an open menu
            // should dismiss it, not open a second one.
            (HitRegion::BlockMenuScrim, _) => {
                self.block_menu = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::PickerRow(i), MouseButton::Left) => {
                if let Some(p) = self.picker.as_mut() {
                    p.selected = i;
                }
                let action = self.picker.as_ref().and_then(|p| p.actions.get(i).cloned());
                if let Some(action) = action {
                    self.run_picker_action(action, false);
                } else {
                    self.mark_chrome_dirty();
                }
            }
            // A click on the panel beside a row chose nothing; swallowing it
            // is what makes a near-miss not a dismissal.
            (HitRegion::PickerPanel, _) => {}
            (HitRegion::PickerScrim, MouseButton::Left) => {
                self.picker = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::PaletteRow(i), MouseButton::Left) => {
                if let Some(p) = self.palette_ui.as_mut() {
                    p.selected = i;
                }
                self.run_palette_selection();
            }
            (HitRegion::PaletteScrim, MouseButton::Left) => {
                self.palette_ui = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::DirPickerRow(i), MouseButton::Left) => {
                if let Some(p) = self.dir_picker.as_mut() {
                    p.selected = i;
                }
                self.dir_picker_activate(i);
            }
            // Outside the panel dismisses; the panel itself swallows, so a
            // click that just misses the entry does not throw away a
            // half-typed path.
            (HitRegion::OpenFileScrim, MouseButton::Left) => {
                self.open_file = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::OpenFilePanel, _) => {}
            (HitRegion::DirPickerScrim, MouseButton::Left) => {
                self.dir_picker = None;
                self.mark_chrome_dirty();
            }
            // A missed click inside the panel must not fall through to the
            // scrim and dismiss what the user is reading.
            (HitRegion::DirPickerPanel | HitRegion::DirPickerRow(_) | HitRegion::DirPickerScrim, _) => {}
            // The Settings* widget regions are the shared §11 vocabulary:
            // while the Profiles screen is up they were drawn by it (the
            // Settings tab's model is not even built then), so they route
            // to the profiles state; otherwise to the settings tab as ever.
            (HitRegion::SettingsRow(i), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                } else if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.mark_chrome_dirty();
            }
            (HitRegion::SettingsToggle(i), MouseButton::Left) => {
                // Select first, then flip through the same path the keyboard
                // uses — one code path per change, however it arrives.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                    self.profiles_adjust(1);
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.adjust_selected_setting(1);
            }
            (HitRegion::SettingsSlider(i), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                } else if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.slider_drag = Some(i);
                self.apply_slider_at(i, self.pointer_pos.0 as f32);
            }
            (HitRegion::SettingsCategory(i), MouseButton::Left) => {
                self.select_settings_category(i);
            }
            (HitRegion::SettingsBrowse(row), MouseButton::Left) => {
                // Recorded, not opened -- see `App::pending_pick`. The row is
                // resolved now rather than later because the list is rebuilt
                // every frame and the index would not survive the dialog.
                let to_profiles = self.profiles_tab_active();
                let field = if to_profiles {
                    self.profiles_field_of_row(row)
                } else {
                    self.settings_field_of_row(row)
                };
                if let Some(field) = field {
                    self.pending_pick = Some(PickRequest { field, to_profiles });
                }
            }

            (HitRegion::SettingsReset(i), MouseButton::Left) => {
                // THE DOT RESETS (§11/§12): delete the key from the file,
                // then reload through the cascade — the file stays the
                // single source of truth, exactly like every other edit.
                // The profiles dot deletes from `[profiles.<name>]`, never
                // the root.
                if self.profiles_tab_active() {
                    self.profiles_reset_row(i);
                    return;
                }
                self.reset_setting_row(i);
            }
            (HitRegion::SettingsEditToml, MouseButton::Left) => {
                self.open_config_externally();
            }
            // Typing already goes to the filter; the pill's click only says
            // "yes, this is where the characters go".
            (HitRegion::SettingsFilter, MouseButton::Left) => {}
            (HitRegion::SettingsSegment(row, opt), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_apply_variant(row, opt);
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.apply_variant(row, opt);
            }
            (HitRegion::SettingsStep(row, up), MouseButton::Left) => {
                // Select first, then step through the keyboard's path — one
                // code path per change, however it arrives.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_adjust(if up { 1 } else { -1 });
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.adjust_selected_setting(if up { 1 } else { -1 });
            }
            (HitRegion::SettingsSelect(row), MouseButton::Left) => {
                // Select first, then act through the keyboard's path (Enter)
                // — one dispatch per widget, however the request arrives.
                // Arming `ui.menu` directly here left the theme pill dead:
                // ThemePicker's options are a roster, not `field.variants`,
                // so the same-pass menu resolution discarded the menu and
                // the click opened nothing. Enter already knows the picker
                // is that widget's dropdown.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_activate_selected();
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.activate_selected_setting();
            }
            (HitRegion::SettingsMenuRow(opt), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    self.profiles_apply_menu_choice(opt);
                    return;
                }
                self.apply_menu_choice(opt);
            }
            // A click inside the menu keeps it: the search box is where the
            // keys already go, so focusing it is swallowing the click, and a
            // near-miss on the panel's padding must not dismiss what it was
            // aiming at. Without these both fall through to the pane.
            (HitRegion::SettingsMenuSearch | HitRegion::SettingsMenuPanel, MouseButton::Left) => {}
            (HitRegion::SettingsMenuFooter, MouseButton::Left) => {
                // "Browse all themes…": the gallery shows the swatches a
                // 288px menu cannot.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.menu = None;
                }
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.menu = None;
                }
                self.show_screen(AppScreen::Themes);
            }
            (HitRegion::SettingsListRemove(row, item), MouseButton::Left) => {
                self.remove_list_item(row, item);
            }
            (HitRegion::SettingsListAdd(row), MouseButton::Left) => {
                self.begin_list_add(row);
            }
            (HitRegion::SettingsListEdit(row, item), MouseButton::Left) => {
                self.begin_list_edit(row, item);
            }
            (HitRegion::SettingsListItem(row, item), MouseButton::Left) => {
                // Drag-to-reorder begins here; crossing another item applies
                // the move (order IS the setting, §11). Release ends it.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                    ui.list_drag = Some((row, item));
                }
                self.mark_chrome_dirty();
            }
            (HitRegion::ProfilesRailRow(i), MouseButton::Left) => {
                // Selecting in the rail EDITS; only the launcher launches —
                // two different verbs, two different places (§12).
                self.profiles_select_rail(i);
            }
            (HitRegion::ProfilesNew, MouseButton::Left) => {
                self.profiles_new();
            }
            (HitRegion::ProfilesName, MouseButton::Left) => {
                self.profiles_begin_rename();
            }
            (HitRegion::ProfilesDuplicate, MouseButton::Left) => {
                self.profiles_duplicate();
            }
            (HitRegion::ProfilesDelete, MouseButton::Left) => {
                self.profiles_delete();
            }
            (HitRegion::ProfilesChoice(row, opt), MouseButton::Left) => {
                self.profiles_choice(row, opt);
            }
            // PalettePanel and SettingsPanel deliberately have no arm: the
            // panels exist in the hit map to swallow clicks, not to act.
            (HitRegion::Drag, MouseButton::Left) => {
                let now = std::time::Instant::now();
                let double = self
                    .last_drag_click
                    .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(400));
                self.last_drag_click = Some(now);
                if let Some(w) = self.window.as_ref() {
                    if double {
                        // Double-click on empty chrome is zoom, matching what
                        // every native macOS titlebar does.
                        w.set_maximized(!w.is_maximized());
                    } else if let Err(e) = w.drag_window() {
                        tracing::debug!(error = %e, "window drag unavailable");
                    }
                }
            }
            (HitRegion::CaptionButton(which), MouseButton::Left) => {
                if let Some(w) = self.window.as_ref().map(Arc::clone) {
                    match which {
                        CaptionButton::Minimize => w.set_minimized(true),
                        CaptionButton::Maximize => w.set_maximized(!w.is_maximized()),
                        // Through the same path the window manager's own close
                        // takes. Anything else and the drawn button quietly
                        // skips `persist_tabs`, so every launch would forget
                        // the tab set — a bug that only appears next time.
                        CaptionButton::Close => self.request_close(),
                    }
                }
            }
            (HitRegion::Resize(edge), MouseButton::Left) => {
                if let Some(w) = self.window.as_ref() {
                    if let Err(e) = w.drag_resize_window(edge.into()) {
                        tracing::debug!(error = %e, "window resize unavailable");
                    }
                }
            }
            _ => {}
        }
    }
}
