//! The Settings tab: reading a value, editing one, and writing it back.
//!
//! Moved out of `app/mod.rs` (#554) as code only -- `SettingsUiState` and
//! `App`'s `settings_ui` field stay on the parent, which reads them from the
//! chrome and keyboard paths.
//!
//! Two rules here are load-bearing and easy to undo. **Leaving a field is a
//! commit** and only Esc discards (#272), which is why `settings_commit_edit`
//! and `settle_list_buffer` exist rather than an Enter arm. And **one parse,
//! shared by add and edit** (#550): `commit_list_append` and
//! `commit_list_replace` go through the same `split_env_entry`, because a rule
//! that accepted `Q=a=b` on an add and rejected it on an edit would leave a row
//! whose entries can be created and then never corrected.
//!
//! The drop handlers stayed behind deliberately. `on_file_dropped` writes into
//! a settings field, so it looks like it belongs -- but a drop is routed by
//! *which screen is open* (#144), which is a window concern, not this tab's.

use super::*;

impl App {
    /// The live value of a field, read from the resolved settings — the same
    /// serialization the rows display, so an edit steps from what is shown.
    pub(super) fn settings_value_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        let ui = self.settings_ui.as_ref()?;
        let field = ui.fields.get(field_idx)?;
        let values = serde_json::to_value(&self.settings).ok()?;
        zest_config::ui::value_at(&values, &field.key).cloned()
    }

    /// Arrow-key editing on the selected row: flip, cycle or step, then write.
    pub(super) fn adjust_selected_setting(&mut self, dir: i32) {
        let Some(idx) = self.selected_settings_field() else { return };
        let Some(current) = self.settings_value_of(idx) else { return };
        // Enumerating installed families is a real scan, so it happens only on
        // the keypress that needs it and only for the field that needs it.
        let installed: Vec<String> = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .filter(|f| f.widget == zest_config::ui::Widget::FontList)
            .and(self.fonts.as_mut())
            .map(Fonts::installed_families)
            .unwrap_or_default();
        let next = self.settings_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            let themes: Vec<String> = crate::themes::ids();
            crate::settings_ui::adjust(field, &current, dir, &themes, &installed)
        });
        if let Some(value) = next {
            self.apply_edit(idx, value);
        }
    }

    /// Enter on the selected row: act on it the way its widget wants.
    pub(super) fn activate_selected_setting(&mut self) {
        use zest_config::ui::Widget;
        // Moving from one field to another is leaving the first one, so it
        // commits like any other exit (#275).
        if !self.settings_commit_edit() {
            return;
        }
        let Some(idx) = self.selected_settings_field() else { return };
        let Some(widget) = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .map(|f| f.widget)
        else {
            return;
        };
        match widget {
            // One keypress, one change: instant for the widgets whose next
            // value is unambiguous — a toggle, or a segmented control.
            Widget::Toggle => {
                self.adjust_selected_setting(1);
            }
            Widget::Select => {
                let segmented = self
                    .settings_ui
                    .as_ref()
                    .and_then(|ui| ui.fields.get(idx))
                    .is_some_and(crate::settings_ui::select_is_segmented);
                if segmented {
                    self.adjust_selected_setting(1);
                } else {
                    // The documented/long selects open their menu (§11) —
                    // the doc comments are the reason the menu exists.
                    let row = self.settings_ui.as_ref().map(|ui| ui.selected);
                    if let (Some(ui), Some(row)) = (self.settings_ui.as_mut(), row) {
                        ui.menu = Some(MenuState::variants(row));
                    }
                }
            }
            // The rosters open the same dropdown the schema selects do,
            // with a search row: stepping is fine for five themes and
            // useless for 266 installed families. The arrows still cycle for
            // anyone who wants them.
            Widget::FontList | Widget::ThemePicker => {
                if !self.open_roster_menu(false) {
                    // Nothing to choose from: fall back to cycling rather than
                    // swallowing the keypress.
                    self.adjust_selected_setting(1);
                }
            }
            // Numbers, text and paths open a buffer: arrows step a number,
            // but "make it 18" should not be nine keypresses, and a string
            // has no other way in.
            Widget::Number | Widget::Slider | Widget::Text | Widget::Path | Widget::FilePath => {
                let seed = self.settings_ui.as_ref().and_then(|ui| {
                    let field = ui.fields.get(idx)?;
                    let values = serde_json::to_value(&self.settings).ok()?;
                    Some(crate::settings_ui::edit_seed(
                        field,
                        zest_config::ui::value_at(&values, &field.key),
                    ))
                });
                if let (Some(ui), Some(buffer)) = (self.settings_ui.as_mut(), seed) {
                    ui.editing = Some(crate::settings_ui::EditBuffer {
                        field_idx: idx,
                        buffer: TextField::new(buffer),
                        error: false,
                        list: crate::settings_ui::ListEdit::Value,
                    });
                }
            }
            // Enter on a list row means "add one" — the add affordance's
            // keyboard spelling.
            Widget::TagList | Widget::KeyValue => {
                let row = self.settings_ui.as_ref().map(|ui| ui.selected);
                if let Some(row) = row {
                    self.begin_list_add(row);
                }
            }
            // The profile pickers belong to the profiles editor (#130),
            // which this tab does not render — when it does, they open
            // rosters the way ThemePicker does.
            Widget::HostPicker
            | Widget::SchemePicker
            | Widget::AccentPicker
            | Widget::IconPicker => {}
        }
        self.mark_chrome_dirty();
    }

    /// Set a slider row's value from a pointer x, against the track the last
    /// layout actually drew.
    ///
    /// Quantized to the arrow keys' grid and applied only when the quantized
    /// value changes — a drag is then at most twenty writes across the whole
    /// travel, not one per motion event.
    pub(super) fn apply_slider_at(&mut self, row: usize, x: f32) {
        self.refresh_chrome();
        let Some(track) = self.chrome_layout.as_ref().and_then(|l| {
            l.settings_tracks.iter().find(|(i, _)| *i == row).map(|(_, r)| *r)
        }) else {
            return;
        };
        let frac = f64::from(((x - track[0]) / track[2]).clamp(0.0, 1.0));
        // Whose track: while the Profiles screen is up, the tracks were
        // drawn by it (the Settings model is not built then — the gate in
        // refresh_chrome is what makes this dispatch unambiguous).
        if self.profiles_tab_active() {
            let Some(field_idx) = self.profiles_field_of_row(row) else { return };
            let candidate = self
                .profiles_ui
                .as_ref()
                .and_then(|ui| ui.fields.get(field_idx))
                .and_then(|field| crate::settings_ui::slider_value(field, frac));
            let Some(candidate) = candidate else { return };
            if self.profiles_value_of(field_idx).as_ref() == Some(&candidate) {
                return;
            }
            self.profiles_apply_edit(field_idx, candidate);
            return;
        }
        let Some(field_idx) = self.settings_ui.as_ref().and_then(|ui| {
            match ui.actions.get(row) {
                Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
                _ => None,
            }
        }) else {
            return;
        };
        let candidate = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(field_idx))
            .and_then(|field| crate::settings_ui::slider_value(field, frac));
        let Some(candidate) = candidate else { return };
        if self.settings_value_of(field_idx).as_ref() == Some(&candidate) {
            return;
        }
        self.apply_edit(field_idx, candidate);
    }

    /// Write one edited setting through to the user's config file, then apply
    /// it by re-running the cascade synchronously.
    ///
    /// The file stays the single source of truth: the overlay never holds a
    /// value the file does not. The watcher will echo this write ~120ms
    /// later; its reload diffs to `Invalidation::None` and is a no-op — the
    /// synchronous reload here is what makes a toggle feel like a switch
    /// rather than a request.
    pub(super) fn apply_edit(&mut self, field_idx: usize, new_value: serde_json::Value) {
        // The one door every edit arrives at, which is why the capability
        // check lives here rather than beside each control. Suppressing the
        // hit regions stops the *pointer*, but the keyboard reaches a row by
        // selection and never touches them -- so a control that is dimmed and
        // unclickable was still adjustable with the arrow keys, which is the
        // same silent write in a different costume.
        let refused = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(field_idx))
            .filter(|f| !self.capabilities().honours(&f.key))
            .map(|f| f.key.clone());
        if let Some(key) = refused {
            self.settings_report(format!("{key} has no effect on this platform, so it was not written"));
            return;
        }
        let Some((key, value)) = self.settings_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(field_idx)?;
            Some((field.key.clone(), crate::settings_ui::to_toml(field, &new_value)?))
        }) else {
            // Unreachable while every widget the walk emits has a `to_toml`
            // arm — which is exactly why it must not be a bare `return`. The
            // next widget added without one would be a control that silently
            // does nothing (#275, the profiles side's #272).
            self.settings_report(format!("this setting cannot be written (field {field_idx})"));
            return;
        };
        // Through `config_target`, like the Profiles side, rather than
        // composing the fallback here: `config_write_target`'s own doc says it
        // exists "rather than a `config_dir().join(CONFIG_FILE)` at each call
        // site", because portable mode's file is `zesterm.toml` beside the
        // binary and a caller that builds the fallback itself writes to a file
        // the loader will never read. This one happened to build it correctly;
        // the next one would not have to.
        let Some(target) = Self::config_target() else {
            self.settings_error = Some("no config directory on this system".to_string());
            self.mark_chrome_dirty();
            return;
        };
        match zest_config::write_value(&target, &key, value) {
            Ok(()) => {
                self.settings_error = None;
                if zest_config::invalidate::class_of(&key) == zest_config::Invalidation::Restart {
                    self.shared.restart_pending.borrow_mut().insert(key);
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, "could not write the setting");
                self.settings_error = Some(format!("could not save {key}: {e}"));
            }
        }
        self.mark_chrome_dirty();
    }

    /// Open the native picker and write whatever comes back.
    ///
    /// Blocking, and on the main thread on purpose: rfd's own guidance is to
    /// spawn dialogs from the main thread, and a background thread would need
    /// the answer marshalled back through the event loop for no gain — nothing
    /// can usefully happen in this window while a modal dialog owns it anyway.
    pub(super) fn run_file_picker(&mut self, request: PickRequest) {
        let PickRequest { field, to_profiles } = request;
        let key = self.picker_field_key(field, to_profiles);

        let mut dialog = rfd::FileDialog::new().set_title("Choose a picture");
        // Start where the setting already points, so replacing a picture opens
        // in the folder the last one came from rather than at the filesystem
        // root.
        if let Some(dir) = self
            .picker_current_value(field, to_profiles)
            .as_deref()
            .and_then(crate::background::resolve_path)
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .filter(|p| p.is_dir())
        {
            dialog = dialog.set_directory(dir);
        }
        // Named rather than a bare extension list so the dialog's own filter
        // dropdown reads as a sentence, and "All files" stays available for a
        // picture whose name says nothing.
        dialog = dialog
            .add_filter("Pictures", &["png", "jpg", "jpeg", "webp", "gif", "bmp", "avif", "tif", "tiff"])
            .add_filter("All files", &["*"]);

        tracing::debug!(?key, "opening the file picker");
        let Some(chosen) = dialog.pick_file() else {
            // Cancelled. Not an error and not worth a banner.
            return;
        };
        let Some(text) = chosen.to_str().map(str::to_string) else {
            self.drop_report(to_profiles, "that path is not valid UTF-8".to_string());
            return;
        };
        // The same gate the drop path applies, and for the same reason: the
        // dialog offers "All files" on purpose — a picture whose name says
        // nothing is exactly what people have — so having been chosen through
        // it is evidence of nothing. Written unchecked, a file that decodes to
        // nothing draws nothing, with the setting saying it should.
        if !crate::background::looks_like_an_image(&chosen) {
            self.drop_report(
                to_profiles,
                format!("{} is not a picture this build can read", chosen.display()),
            );
            return;
        }
        // Through the same write path a typed edit takes, so provenance, the
        // restart ledger and the reload all behave identically -- a picker
        // that wrote the file itself would be a second way to save a setting.
        if to_profiles {
            self.profiles_apply_edit(field, serde_json::Value::String(text));
        } else {
            self.apply_edit(field, serde_json::Value::String(text));
        }
    }

    /// The dotted key a pick will write, for the log line only.
    fn picker_field_key(&self, field: usize, to_profiles: bool) -> Option<String> {
        let fields = if to_profiles {
            self.profiles_ui.as_ref().map(|ui| &ui.fields)
        } else {
            self.settings_ui.as_ref().map(|ui| &ui.fields)
        };
        fields?.get(field).map(|f| f.key.clone())
    }

    /// What the setting says today, so the dialog can open beside it.
    fn picker_current_value(&self, field: usize, to_profiles: bool) -> Option<String> {
        let value = if to_profiles {
            self.profiles_value_of(field)
        } else {
            self.settings_value_of(field)
        };
        value?.as_str().map(str::to_string)
    }

    /// Say why nothing happened — the Settings tab's `profiles_report`.
    pub(super) fn settings_report(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::error!(reason = %message, "the settings editor wrote nothing");
        self.settings_error = Some(message);
        self.mark_chrome_dirty();
    }

    /// Commit an open settings edit, if there is one.
    ///
    /// `true` means the caller may proceed. `false` means a buffer is open and
    /// does not parse (or an append was rejected): it stays on screen with its
    /// warn border and the caller must NOT move — leaving would destroy it,
    /// which is the whole of #275.
    pub(super) fn settings_commit_edit(&mut self) -> bool {
        let pending = match self.settings_ui.as_mut() {
            Some(ui) => crate::settings_ui::take_pending_edit(&mut ui.editing, &ui.fields),
            None => return true,
        };
        match pending {
            crate::settings_ui::Pending::None => true,
            crate::settings_ui::Pending::Refused => {
                self.mark_chrome_dirty();
                false
            }
            crate::settings_ui::Pending::Commit(idx, value) => {
                self.apply_edit(idx, value);
                true
            }
            // The add-chip and the entry edit: `commit_list_append` and
            // `commit_list_replace` own the parse, because either needs the
            // list's current value. Both leave the buffer open, so both are
            // settled in one place.
            crate::settings_ui::Pending::Append(idx, text) => {
                let took = self.commit_list_append(idx, &text);
                self.settle_list_buffer(took);
                took
            }
            crate::settings_ui::Pending::Replace(idx, item, text) => {
                let took = self.commit_list_replace(idx, item, &text);
                self.settle_list_buffer(took);
                took
            }
        }
    }

    /// Settle the buffer an append or a replace left open.
    ///
    /// Shared by both editors, because the profiles tab had none: its
    /// `Append` arm returned `commit_list_append`'s answer and left the typed
    /// text sitting in the row it had just written, with a refusal showing no
    /// error at all. One settle, so the two tabs cannot disagree about what
    /// happens after a list edit.
    pub(super) fn settle_list_buffer(&mut self, took: bool) {
        let editing = if self.profiles_tab_active() {
            self.profiles_ui.as_mut().map(|ui| &mut ui.editing)
        } else {
            self.settings_ui.as_mut().map(|ui| &mut ui.editing)
        };
        if let Some(editing) = editing {
            if took {
                *editing = None;
            } else if let Some(edit) = editing.as_mut() {
                edit.error = true;
            }
        }
        self.mark_chrome_dirty();
    }

    /// Where an edit lands: the user's config file, existing or about to.
    ///
    /// One line, and it stays a wrapper rather than being inlined at its call
    /// sites: the portable-mode ordering inside it is the part that is easy to
    /// get wrong, and it now lives once, in `zest-config`, where the daemon
    /// reads it too.
    pub(super) fn config_target() -> Option<std::path::PathBuf> {
        zest_config::paths::config_write_target()
    }

    /// The field index a settings row stands for, when it is a real field.
    pub(super) fn settings_field_of_row(&self, row: usize) -> Option<usize> {
        match self.settings_ui.as_ref()?.actions.get(row) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }

    /// The rail's visible categories under the live filter — the click
    /// handler resolves `SettingsCategory(i)` against exactly what the model
    /// showed, or filtered-away rows would take clicks for their neighbours.
    pub(super) fn visible_categories(&self) -> Vec<String> {
        use crate::settings_ui as sui;
        let Some(ui) = self.settings_ui.as_ref() else { return Vec::new() };
        sui::categories(&ui.fields)
            .into_iter()
            .filter(|g| {
                // Only a live filter hides categories (§11) — the unknown
                // category included: clean, unfiltered, it stays in the rail
                // and its page carries the empty-state line.
                ui.filter.is_empty()
                    || sui::category_matches(&ui.fields, g, ui.filter.text(), &self.unknown_keys)
            })
            .collect()
    }

    /// Select the rail's `i`-th visible category; selection and scroll reset
    /// — a category is a fresh page, not a continuation.
    pub(super) fn select_settings_category(&mut self, i: usize) {
        let Some(label) = self.visible_categories().get(i).cloned() else { return };
        let moving = self.settings_ui.as_ref().is_some_and(|ui| ui.category != label);
        // Commit before moving, and stay put if the buffer cannot be written:
        // changing category used to drop it silently (#275).
        if moving && !self.settings_commit_edit() {
            return;
        }
        if let Some(ui) = self.settings_ui.as_mut() {
            if ui.category != label {
                ui.category = label;
                ui.selected = 0;
                ui.scroll = 0.0;
                ui.scroll_to_selected = true;
                ui.menu = None;
            }
        }
        self.mark_chrome_dirty();
    }

    /// The modified dot's click: delete the key from the file, reload — the
    /// dot is the reset button (§11), and the file stays the single source
    /// of truth. Idempotent because `remove_value` is.
    pub(super) fn reset_setting_row(&mut self, row: usize) {
        let Some(key) = self
            .settings_field_of_row(row)
            .and_then(|i| self.settings_ui.as_ref()?.fields.get(i).map(|f| f.key.clone()))
        else {
            return;
        };
        let Some(target) = Self::config_target() else {
            self.settings_error = Some("no config directory on this system".to_string());
            self.mark_chrome_dirty();
            return;
        };
        match zest_config::remove_value(&target, &key) {
            Ok(()) => {
                self.settings_error = None;
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, "could not reset the setting");
                self.settings_error = Some(format!("could not reset {key}: {e}"));
            }
        }
        self.mark_chrome_dirty();
    }

    /// "Edit as TOML": hand the config file to the OS handler.
    pub(super) fn open_config_externally(&mut self) {
        let Some(target) = Self::config_target() else { return };
        platform::open_path(&target);
    }

    /// Write one of a select field's variants — segmented segments and
    /// dropdown rows both land here, and from here in `apply_edit`.
    pub(super) fn apply_variant(&mut self, row: usize, opt: usize) {
        let Some(idx) = self.settings_field_of_row(row) else { return };
        self.apply_variant_at(idx, opt);
    }

    /// The same, by field index — what the dropdown has after it resolves its
    /// row, and what keeps `apply_menu_choice` from resolving it twice.
    pub(super) fn apply_variant_at(&mut self, idx: usize, opt: usize) {
        let Some(value) = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .and_then(|f| f.variants.get(opt))
            .map(|v| serde_json::Value::String(v.value.clone()))
        else {
            return;
        };
        self.apply_edit(idx, value);
    }

    /// A list item's ×: fonts and tags lose the item, an env entry loses its
    /// key. The whole new value goes through `apply_edit` — no second path.
    ///
    /// **Both editors, dispatched on which tab is showing.** The profiles
    /// editor draws its controls with the same `ss::draw_control`, so a
    /// KeyValue row there pushes the same `SettingsListAdd`/`Remove` regions —
    /// and while this read `settings_ui` unconditionally, those chips drew and
    /// did nothing on the profiles tab. A control that silently does nothing
    /// is the #272 class; the §12 `env` row was shipping as one (#496).
    pub(super) fn remove_list_item(&mut self, row: usize, item: usize) {
        if self.profiles_tab_active() {
            let Some(idx) = self.profiles_field_of_row(row) else { return };
            let Some((widget, key)) = self
                .profiles_ui
                .as_ref()
                .and_then(|ui| ui.fields.get(idx))
                .map(|f| (f.widget, f.key.clone()))
            else {
                return;
            };
            let Some(current) = self.profiles_value_of(idx) else { return };
            // `env` is the one row whose drawn map and written map differ: the
            // x sits beside a *merged* entry and the file holds only the
            // profile's own table, so the index is resolved against one and
            // the value written to the other (#550). Keyed on the field rather
            // than on `Widget::KeyValue`, because a second key-value row here
            // would not want this and would say so by its name.
            let next = if key == "env" {
                let Some(own) = self.profiles_seed_of(idx) else { return };
                env_value_without(&current, &own, item)
            } else {
                list_value_without(widget, &current, item)
            };
            let Some(next) = next else { return };
            self.profiles_apply_edit(idx, next);
            return;
        }
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return;
        };
        let Some(current) = self.settings_value_of(idx) else { return };
        let Some(next) = list_value_without(widget, &current, item) else { return };
        self.apply_edit(idx, next);
    }

    /// The dashed add affordance: fonts open the existing value picker (in
    /// append mode); tags and env entries open a typed buffer whose Enter
    /// appends.
    pub(super) fn begin_list_add(&mut self, row: usize) {
        use zest_config::ui::Widget;
        if self.profiles_tab_active() {
            if !self.profiles_commit_edit() {
                return;
            }
            let Some(idx) = self.profiles_field_of_row(row) else { return };
            let widget =
                self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget);
            // No `FontList` arm: §12 offers no roster field, and opening the
            // Settings roster menu from here would edit the wrong document.
            if widget == Some(Widget::KeyValue) || widget == Some(Widget::TagList) {
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.selected = row;
                    ui.editing = Some(crate::settings_ui::EditBuffer {
                        field_idx: idx,
                        buffer: TextField::default(),
                        error: false,
                        list: crate::settings_ui::ListEdit::Append,
                    });
                }
            }
            self.mark_chrome_dirty();
            return;
        }
        if !self.settings_commit_edit() {
            return;
        }
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return;
        };
        if let Some(ui) = self.settings_ui.as_mut() {
            ui.selected = row;
        }
        match widget {
            Widget::FontList => {
                self.open_roster_menu(true);
            }
            Widget::TagList | Widget::KeyValue => {
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.editing = Some(crate::settings_ui::EditBuffer {
                        field_idx: idx,
                        buffer: TextField::default(),
                        error: false,
                        list: crate::settings_ui::ListEdit::Append,
                    });
                }
            }
            _ => {}
        }
        self.mark_chrome_dirty();
    }

    /// Click an entry: edit it where it is drawn.
    ///
    /// The gap this closes is that no list widget had an edit affordance at
    /// all -- changing one variable meant removing it and retyping both
    /// halves, and the font row's per-item region already meant something
    /// else (drag-to-reorder).
    ///
    /// Seeded from what the row *drew*, which on the profiles tab is the
    /// merged environment: the person clicked an entry showing a value, so
    /// the buffer has to open on that value. Editing an inherited one is how
    /// you override it, and `commit_list_replace` lands the result in the
    /// profile's own table.
    pub(super) fn begin_list_edit(&mut self, row: usize, item: usize) {
        let profiles = self.profiles_tab_active();
        // Leaving the open buffer is a commit, and a refused one must keep
        // the field (#272/#275) -- so the old edit settles before a new one
        // opens, exactly as the add chip does.
        let free = if profiles { self.profiles_commit_edit() } else { self.settings_commit_edit() };
        if !free {
            return;
        }
        let Some(idx) =
            (if profiles { self.profiles_field_of_row(row) } else { self.settings_field_of_row(row) })
        else {
            return;
        };
        let ui_field = if profiles {
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        } else {
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        };
        let Some(widget) = ui_field else { return };
        let current =
            if profiles { self.profiles_value_of(idx) } else { self.settings_value_of(idx) };
        let Some(current) = current else { return };
        let Some(text) = list_entry_text(widget, &current, item) else { return };
        let edit = crate::settings_ui::EditBuffer {
            field_idx: idx,
            buffer: TextField::new(&text),
            error: false,
            list: crate::settings_ui::ListEdit::Replace(item),
        };
        if profiles {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.selected = row;
                ui.editing = Some(edit);
            }
        } else if let Some(ui) = self.settings_ui.as_mut() {
            ui.selected = row;
            ui.editing = Some(edit);
        }
        self.mark_chrome_dirty();
    }

    /// Commit a replace buffer: the entry at `item` becomes `text`.
    ///
    /// `commit_list_append`'s twin, deliberately down to its shape -- same
    /// seed on the profiles tab, same "empty closes the buffer rather than
    /// erroring", same `false` meaning the input cannot be an entry. A rule
    /// that differed between add and edit would be a row whose entries can be
    /// created and then not corrected.
    pub(super) fn commit_list_replace(&mut self, idx: usize, item: usize, text: &str) -> bool {
        let profiles = self.profiles_tab_active();
        let field = if profiles {
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| (f.widget, f.key.clone()))
        } else {
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| (f.widget, f.key.clone()))
        };
        let Some((widget, key)) = field else { return false };
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        if profiles {
            let Some(drawn) = self.profiles_value_of(idx) else { return false };
            // `env` again: the index names an entry in the merged map the row
            // drew, and the value is written to the profile's own table.
            let next = if key == "env" {
                let Some(own) = self.profiles_seed_of(idx) else { return false };
                env_value_replacing(&drawn, &own, &self.profiles_defaults_env(), item, text)
            } else {
                list_value_replacing(widget, &drawn, item, text)
            };
            let Some(next) = next else { return false };
            self.profiles_apply_edit(idx, next);
            return true;
        }
        let Some(current) = self.settings_value_of(idx) else { return false };
        let Some(next) = list_value_replacing(widget, &current, item, text) else { return false };
        self.apply_edit(idx, next);
        true
    }

    /// Commit an append buffer: a tag verbatim (a leading `-` disables and
    /// is kept), or a `KEY=VALUE` env entry — a bare `KEY` gets an empty
    /// value, which *unsets* under the wholesale-replace semantics. Returns
    /// false when the input cannot be an entry (shown as a buffer error).
    pub(super) fn commit_list_append(&mut self, idx: usize, text: &str) -> bool {
        let profiles = self.profiles_tab_active();
        let widget = if profiles {
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        } else {
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        };
        let Some(widget) = widget else { return false };
        let text = text.trim();
        if text.is_empty() {
            // Committing nothing is closing the buffer, not an error.
            return true;
        }
        // The *seed*, not the displayed value, on the profiles tab: an append
        // writes the profile's own table, and appending to the merged
        // environment stores every inherited variable as this profile's own
        // (#550). Identical to `profiles_value_of` for every other widget --
        // `edit_seed_value` only diverges for `command`, `host` and `env`.
        let current =
            if profiles { self.profiles_seed_of(idx) } else { self.settings_value_of(idx) };
        let Some(current) = current else { return false };
        let Some(next) = list_value_with(widget, &current, text) else { return false };
        if profiles {
            self.profiles_apply_edit(idx, next);
        } else {
            self.apply_edit(idx, next);
        }
        true
    }

    /// Drag-to-reorder on a font row: move `from` to `to` — order is the
    /// setting, so the move writes through the file like every other edit.
    pub(super) fn reorder_list_item(&mut self, row: usize, from: usize, to: usize) {
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(current) = self.settings_value_of(idx) else { return };
        let Some(arr) = current.as_array() else { return };
        if from >= arr.len() || to >= arr.len() || from == to {
            return;
        }
        let mut arr = arr.clone();
        let moved = arr.remove(from);
        arr.insert(to, moved);
        self.apply_edit(idx, serde_json::Value::Array(arr));
    }
}

#[cfg(test)]
mod profile_env_round_trip_tests {
    //! The env row is the one control whose map on screen and map in the file
    //! are different maps, and every bug in #550 is that difference going
    //! unnoticed. These close the loop the unit tests above cannot: apply the
    //! editor's transformation, store the result the way `write_profile_value`
    //! does, resolve again, and look at what a launch would actually get.

    use super::{env_value_replacing, env_value_without, list_value_with};
    use zest_config::profiles::resolve_profile;
    use zest_config::ui::Widget;

    /// `[profiles.defaults.env] SHARED` plus `[profiles.work.env] OWN`.
    fn config() -> toml::Table {
        "[profiles.defaults.env]
SHARED = \"1\"
[profiles.work.env]
OWN = \"2\"
"
            .parse()
            .expect("valid toml")
    }

    fn as_json(map: &std::collections::BTreeMap<String, String>) -> serde_json::Value {
        serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        )
    }

    /// What the row draws: the merged environment the launch will use.
    fn merged(c: &toml::Table) -> serde_json::Value {
        as_json(&resolve_profile(c, "work").meta.env)
    }

    /// What the file holds: `[profiles.work.env]` alone.
    fn own(c: &toml::Table) -> serde_json::Value {
        as_json(&resolve_profile(c, "work").own_env)
    }

    /// `write_profile_value(.., "work", "env", ..)` without a disk: replace
    /// `[profiles.work].env` wholesale, which is what an `InlineTable` write
    /// does.
    fn store(c: &toml::Table, next: &serde_json::Value) -> toml::Table {
        let mut c = c.clone();
        let table = next
            .as_object()
            .expect("an env value is an object")
            .iter()
            .map(|(k, v)| {
                (k.clone(), toml::Value::String(v.as_str().expect("a string").to_string()))
            })
            .collect::<toml::Table>();
        c.get_mut("profiles")
            .and_then(toml::Value::as_table_mut)
            .and_then(|p| p.get_mut("work"))
            .and_then(toml::Value::as_table_mut)
            .expect("[profiles.work] exists")
            .insert("env".into(), toml::Value::Table(table));
        c
    }

    /// Position of a key in the drawn row, which renders in map order.
    fn index_of(value: &serde_json::Value, key: &str) -> usize {
        value.as_object().expect("an object").keys().position(|k| k == key).expect("a drawn entry")
    }

    #[test]
    fn removing_an_inherited_entry_is_not_answered_by_defaults_putting_it_back() {
        // The reported symptom, and the sharpest of the five: the x on an
        // inherited variable visibly does nothing. Reading the *merged* map
        // and storing it back writes SHARED out of a table it was never in,
        // and `fold_meta` merges Defaults in again on the very next read.
        //
        // The design's answer is per-variable rather than all-or-nothing
        // (client-ui README, "An empty value unsets a variable"): drop an
        // inherited entry by writing it empty into the profile's own table.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "SHARED");

        let next = env_value_without(&drawn, &own(&c), item).expect("a new value");
        let after = merged(&store(&c, &next));
        assert_eq!(
            after.get("SHARED").and_then(|v| v.as_str()),
            Some(""),
            "removing an inherited entry must unset it; anything else and the x did nothing"
        );
        assert_eq!(
            after.get("OWN").and_then(|v| v.as_str()),
            Some("2"),
            "and it must not disturb the entries beside it"
        );
    }

    #[test]
    fn adding_an_entry_does_not_hard_copy_defaults_into_the_profile() {
        // The same read, failing in the opposite direction. Storing the merged
        // map makes SHARED the profile's own, so it stops tracking Defaults --
        // silently, and visible only the next time Defaults changes.
        let c = config();

        let next = list_value_with(Widget::KeyValue, &own(&c), "NEW=3").expect("a new value");
        let stored = store(&c, &next);
        let after = resolve_profile(&stored, "work");
        assert_eq!(
            after.own_env.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["NEW", "OWN"],
            "the profile owns what it named plus the new entry, and nothing inherited"
        );
        assert_eq!(
            after.meta.env.get("SHARED").map(String::as_str),
            Some("1"),
            "while the launch still sees Defaults' entry, through inheritance"
        );
    }

    #[test]
    fn editing_an_inherited_entry_creates_an_override_for_it() {
        // What the gesture can only mean: the person clicked a row showing a
        // value and typed a different one. The index names an entry in the
        // merged map the row drew; the result lands in the profile's own
        // table, so the edit becomes an override rather than a no-op.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "SHARED");

        let next = env_value_replacing(&drawn, &own(&c), &defaults_env(&c), item, "SHARED=mine")
            .expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert_eq!(
            after.own_env.get("SHARED").map(String::as_str),
            Some("mine"),
            "the profile now owns the key it edited"
        );
        assert_eq!(
            after.meta.env.get("OWN").map(String::as_str),
            Some("2"),
            "and its own entries are untouched"
        );
    }

    /// Defaults' own entries, which is what Defaults' env *is*.
    fn defaults_env(c: &toml::Table) -> serde_json::Value {
        as_json(&resolve_profile(c, "defaults").own_env)
    }

    #[test]
    fn renaming_an_entry_does_not_leave_the_old_key_behind() {
        // The failure a naive insert would ship: editing `OWN=2` into
        // `RENAMED=2` has to take the old key with it, or one edit becomes
        // two variables and the shell inherits a name nobody typed.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "OWN");

        let next = env_value_replacing(&drawn, &own(&c), &defaults_env(&c), item, "RENAMED=2")
            .expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert_eq!(after.own_env.get("RENAMED").map(String::as_str), Some("2"));
        assert!(!after.own_env.contains_key("OWN"), "the old key goes with the rename");
        assert!(
            !after.meta.env.contains_key("OWN"),
            "and nothing underneath puts it back -- Defaults never named it"
        );
    }

    #[test]
    fn renaming_an_inherited_entry_takes_the_old_key_with_it() {
        // Review caught this one. Removing the old key from the profile's own
        // table is a no-op when the key was never in it, so `SHARED` kept
        // arriving from Defaults beside the new name: one rename, two
        // variables, and the PR claiming the opposite.
        //
        // The tombstone is the same empty value the x writes, so the state is
        // one the row can draw and the person can undo.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "SHARED");

        let next = env_value_replacing(&drawn, &own(&c), &defaults_env(&c), item, "RENAMED=9")
            .expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert_eq!(after.meta.env.get("RENAMED").map(String::as_str), Some("9"));
        assert_eq!(
            after.meta.env.get("SHARED").map(String::as_str),
            Some(""),
            "the inherited name it was renamed away from must not still reach the shell"
        );
    }

    #[test]
    fn renaming_an_override_of_an_inherited_key_takes_it_with_it_too() {
        // The same hole one layer along: the profile *owns* the key, so the
        // remove lands -- and Defaults names it as well, so it comes back
        // anyway. Ownership is not the question; whether anything underneath
        // still answers for the old name is.
        let c: toml::Table = "[profiles.defaults.env]\nBOTH = \"theirs\"\n\
             [profiles.work.env]\nBOTH = \"mine\"\n"
            .parse()
            .expect("valid toml");
        let drawn = merged(&c);
        let item = index_of(&drawn, "BOTH");

        let next = env_value_replacing(&drawn, &own(&c), &defaults_env(&c), item, "RENAMED=mine")
            .expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert_eq!(after.meta.env.get("RENAMED").map(String::as_str), Some("mine"));
        assert_eq!(
            after.meta.env.get("BOTH").map(String::as_str),
            Some(""),
            "Defaults' value must not resurface under the name the rename left"
        );
    }

    #[test]
    fn renaming_on_defaults_writes_no_tombstone() {
        // Defaults has no layer under it -- `resolve_profile` gives it an
        // empty parent on purpose -- so a removed key is simply gone, and a
        // tombstone would leave an `unset` row over a variable nothing sets.
        // The caller passes an empty map for exactly this; pinned here so the
        // fix for the case above cannot quietly acquire a second bug.
        let c = config();
        let drawn = defaults_env(&c);
        let item = index_of(&drawn, "SHARED");
        let empty = serde_json::Value::Object(serde_json::Map::new());

        let next = env_value_replacing(&drawn, &drawn, &empty, item, "RENAMED=1")
            .expect("a new value");
        let map = next.as_object().expect("an object");
        assert!(map.contains_key("RENAMED"));
        assert!(!map.contains_key("SHARED"), "no tombstone where nothing can resurface: {map:?}");
    }

    #[test]
    fn editing_a_value_in_place_is_not_a_rename() {
        // The guard the tombstone needs: changing `SHARED=1` to `SHARED=9` on
        // a profile that inherits it must produce an override, not an
        // override plus an unset of the same key.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "SHARED");

        let next = env_value_replacing(&drawn, &own(&c), &defaults_env(&c), item, "SHARED=9")
            .expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert_eq!(
            after.meta.env.get("SHARED").map(String::as_str),
            Some("9"),
            "same key, new value -- nothing to take with it"
        );
    }

    #[test]
    fn the_real_writer_lands_an_unset_in_the_profiles_own_table() {
        // The three tests above model `write_profile_value` with a `store`
        // helper, and a helper is exactly what this repo has been caught by
        // before -- a synthetic stand-in kept a broken fix green (ADR-013's
        // "a capture beats a helper"). So one of them runs the production
        // path end to end: the real `to_toml`, the real writer, a real file,
        // re-read and re-resolved.
        let field = zest_config::profiles::fields()
            .into_iter()
            .find(|f| f.key == "env")
            .expect("env is a profile field");

        // Per-process, because this box runs parallel worktrees and libtest
        // runs these concurrently: a fixed name in the shared temp directory
        // is two runs writing one file, which reads as a flaky assertion
        // rather than as a collision (#540 is that shape).
        let path = std::env::temp_dir()
            .join(format!("zesterm-env-editor-round-trip-550-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "# mine
[profiles.defaults.env]
SHARED = \"1\"
             [profiles.work]
# the work one
command = \"pwsh\"
             [profiles.work.env]
OWN = \"2\"
",
        )
        .expect("write");

        let c: toml::Table =
            std::fs::read_to_string(&path).expect("read").parse().expect("valid toml");
        let drawn = merged(&c);
        let item = index_of(&drawn, "SHARED");
        let next = env_value_without(&drawn, &own(&c), item).expect("a new value");

        let value = crate::settings_ui::to_toml(&field, &next).expect("a writable value");
        // Written bare, under `[profiles.work]` -- `env` is a profile-only key,
        // not a dotted settings key, and `write_profile_value` never splits it.
        zest_config::write_profile_value(&path, "work", &field.key, value)
            .expect("the write lands");

        let text = std::fs::read_to_string(&path).expect("read back");
        let after: toml::Table = text.parse().expect("still valid toml");
        let resolved = zest_config::profiles::resolve_profile(&after, "work");
        assert_eq!(
            resolved.meta.env.get("SHARED").map(String::as_str),
            Some(""),
            "the inherited entry is unset for the launch, not restored by Defaults: {text}"
        );
        assert_eq!(
            resolved.meta.env.get("OWN").map(String::as_str),
            Some("2"),
            "and the profile's own entry is untouched: {text}"
        );
        assert!(text.contains("# mine"), "comments elsewhere survive the write: {text}");
        assert!(text.contains("# the work one"), "and the profile's own: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_an_entry_the_profile_owns_deletes_it_from_the_profiles_table() {
        // The branch that already worked, pinned so the fix for the other one
        // cannot turn every remove into an empty value.
        let c = config();
        let drawn = merged(&c);
        let item = index_of(&drawn, "OWN");

        let next = env_value_without(&drawn, &own(&c), item).expect("a new value");
        let after = resolve_profile(&store(&c, &next), "work");
        assert!(
            !after.own_env.contains_key("OWN"),
            "an owned entry is deleted, not emptied -- emptying it would unset a variable              the shell might otherwise inherit"
        );
        assert_eq!(
            after.meta.env.get("SHARED").map(String::as_str),
            Some("1"),
            "and Defaults is untouched either way"
        );
    }
}

#[cfg(test)]
mod list_value_tests {
    use super::{list_entry_text, list_value_replacing, list_value_with, list_value_without};
    use zest_config::ui::Widget;

    fn map(pairs: &[(&str, &str)]) -> serde_json::Value {
        serde_json::Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), serde_json::Value::String((*v).to_string())))
                .collect(),
        )
    }

    #[test]
    fn an_entry_is_replaced_in_place_by_the_same_rule_that_adds_one() {
        // Add and edit share `split_env_entry` deliberately: a rule that
        // accepted `Q=a=b` on an add and rejected it on an edit would be a
        // row whose entries can be created and then never corrected.
        let current = map(&[("A", "1"), ("B", "2")]);

        let edited =
            list_value_replacing(Widget::KeyValue, &current, 0, "A=9").expect("a new value");
        assert_eq!(edited["A"], "9", "the value changes");
        assert_eq!(edited["B"], "2", "and its neighbour does not");

        let renamed =
            list_value_replacing(Widget::KeyValue, &current, 0, "Z=9").expect("a new value");
        assert!(renamed.get("A").is_none(), "a rename takes the old key with it");
        assert_eq!(renamed["Z"], "9");

        let bare = list_value_replacing(Widget::KeyValue, &current, 0, "A").expect("a new value");
        assert_eq!(bare["A"], "", "a bare key unsets, exactly as it does on an add");

        assert!(
            list_value_replacing(Widget::KeyValue, &current, 0, "=orphan").is_none(),
            "an entry with no key is refused on an edit too"
        );
        assert!(
            list_value_replacing(Widget::KeyValue, &current, 9, "A=1").is_none(),
            "and a position nothing was drawn at is nothing to do"
        );

        let tags = serde_json::json!(["-liga", "ss01"]);
        let edited = list_value_replacing(Widget::TagList, &tags, 1, "ss02").expect("a new value");
        assert_eq!(edited, serde_json::json!(["-liga", "ss02"]), "a tag replaces by position");
        assert!(
            list_value_replacing(Widget::FontList, &tags, 0, "Consolas").is_none(),
            "a font family is chosen from the roster, never typed -- and its per-item \
             region already means drag-to-reorder"
        );
    }

    #[test]
    fn an_entry_seeds_its_edit_with_what_the_row_drew() {
        // The buffer has to open on the entry the person clicked, spelled the
        // way the add chip would accept it, so one parse serves both.
        let current = map(&[("A", "1"), ("B", "")]);
        assert_eq!(list_entry_text(Widget::KeyValue, &current, 0).as_deref(), Some("A=1"));
        assert_eq!(
            list_entry_text(Widget::KeyValue, &current, 1).as_deref(),
            Some("B="),
            "an unset entry seeds the spelling that keeps it unset, not the word `unset`"
        );
        let tags = serde_json::json!(["-liga"]);
        assert_eq!(list_entry_text(Widget::TagList, &tags, 0).as_deref(), Some("-liga"));
        assert_eq!(list_entry_text(Widget::KeyValue, &current, 9), None);
    }

    #[test]
    fn a_key_value_entry_is_added_by_key_and_removed_by_position() {
        // One copy of the rule, shared by the Settings tab and the §12
        // profiles tab (#496): they draw these rows with the same
        // `draw_control`, so they have to agree about what its controls do.
        let current = map(&[("A", "1"), ("B", "2")]);

        let added = list_value_with(Widget::KeyValue, &current, "C=3").expect("an entry");
        assert_eq!(added["C"], serde_json::json!("3"));
        assert_eq!(added["A"], serde_json::json!("1"), "the others are untouched");

        // A bare key is the empty-value spelling, which unsets all the way
        // down to the pty rather than setting an empty string.
        let bare = list_value_with(Widget::KeyValue, &current, "C").expect("an entry");
        assert_eq!(bare["C"], serde_json::json!(""));

        // `=` inside the value survives: only the first splits.
        let url = list_value_with(Widget::KeyValue, &current, "Q=a=b").expect("an entry");
        assert_eq!(url["Q"], serde_json::json!("a=b"));

        assert!(
            list_value_with(Widget::KeyValue, &current, "=orphan").is_none(),
            "a nameless entry is refused, and the caller shows that as a buffer error"
        );

        // Removal is by *position*, because that is what the × was drawn
        // beside — the control renders the map in iteration order.
        let removed = list_value_without(Widget::KeyValue, &current, 0).expect("a value");
        assert!(removed.get("A").is_none() && removed.get("B").is_some());
        assert!(
            list_value_without(Widget::KeyValue, &current, 9).is_none(),
            "a stale index is nothing to do, never a write"
        );
    }

    #[test]
    fn a_widget_with_no_list_behaviour_refuses_both() {
        // `None` means "nothing to do" and must never be written as null: a
        // Text row's × or add chip cannot exist, and if one ever reaches here
        // the answer is to do nothing rather than blank the value.
        let current = serde_json::Value::String("x".into());
        assert!(list_value_with(Widget::Text, &current, "y").is_none());
        assert!(list_value_without(Widget::Text, &current, 0).is_none());
    }
}

#[cfg(test)]
mod settings_ime_tests {
    use super::{SettingsUiState, TextCommand, TextField};

    fn state() -> SettingsUiState {
        SettingsUiState {
            selected: 3,
            category: "Appearance".into(),
            filter: TextField::default(),
            scroll: 0.0,
            scroll_to_selected: false,
            actions: Vec::new(),
            fields: Vec::new(),
            editing: None,
            installed: Vec::new(),
            menu: None,
            list_drag: None,
        }
    }

    #[test]
    fn composed_text_lands_in_the_filter_like_a_keystroke() {
        // The review finding this seam exists for: `on_ime` wrote the commit
        // into `tabs.active_source()` — the concealed session — while the
        // Settings tab held the keyboard, so an IME user typing into the
        // filter typed into the hidden shell instead.
        let mut ui = state();
        ui.commit_text("設定");
        assert_eq!(ui.filter.text(), "設定", "no edit open: the filter is where characters go");
        assert_eq!(ui.selected, 0, "a filter edit resets the selection, exactly like typing");
        assert!(ui.scroll_to_selected, "and brings it into view");
    }

    #[test]
    fn composed_text_feeds_an_open_edit_buffer_before_the_filter() {
        let mut ui = state();
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx: 0,
            buffer: TextField::new("nu "),
            error: true,
            list: crate::settings_ui::ListEdit::Value,
        });
        ui.commit_text("シェル");
        let edit = ui.editing.as_ref().expect("still editing");
        assert_eq!(
            edit.buffer.text(),
            "nu シェル",
            "a typed edit owns the characters, as the key path says"
        );
        assert!(!edit.error, "new input clears a stale parse error, as typing does");
        assert!(ui.filter.is_empty(), "nothing leaks into the filter");
    }

    #[test]
    fn an_open_dropdown_swallows_composed_text() {
        // The key path's dropdown arm ignores `Key::Character`; the IME
        // route must agree, or a commit would edit a filter the user cannot
        // see behind the menu.
        let mut ui = state();
        ui.menu = Some(super::MenuState::variants(1));
        ui.commit_text("あ");
        assert!(ui.filter.is_empty(), "the menu owns the keys");
        assert!(ui.editing.is_none());
    }

    #[test]
    fn paste_lands_in_an_open_edit_buffer() {
        // The reported bug (#251): ⌘V in a settings text field did nothing at
        // all. The guard that makes a chord "not text" is `super_key()`,
        // which is also the paste chord, and the `return` under it stopped
        // the global keymap table from ever seeing the key.
        let mut ui = state();
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx: 0,
            buffer: TextField::new("/bin/"),
            error: true,
            list: crate::settings_ui::ListEdit::Value,
        });
        ui.text_key(TextCommand::Paste, Some("zsh"));
        let edit = ui.editing.as_ref().expect("still editing");
        assert_eq!(edit.buffer.text(), "/bin/zsh", "the clipboard landed at the caret");
        assert!(!edit.error, "and cleared the stale parse error, as typing does");
        assert!(ui.filter.is_empty(), "nothing leaked into the filter");
    }

    #[test]
    fn paste_lands_in_the_filter_when_no_edit_is_open() {
        let mut ui = state();
        ui.text_key(TextCommand::Paste, Some("font"));
        assert_eq!(ui.filter.text(), "font", "the filter is where text goes with no buffer open");
        assert_eq!(ui.selected, 0, "a filter edit resets the selection, exactly like typing");
        assert!(ui.scroll_to_selected, "and brings it into view");
    }

    #[test]
    fn a_copy_hands_the_selection_back_to_the_caller() {
        // The app owns the clipboard handle, so `text_key` returns the text
        // to write rather than writing it — the same seam paste comes in on.
        let mut ui = state();
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx: 0,
            buffer: TextField::new("obsidian"),
            error: false,
            list: crate::settings_ui::ListEdit::Value,
        });
        if let Some(edit) = ui.editing.as_mut() {
            edit.buffer.select_all();
        }
        assert_eq!(
            ui.text_key(TextCommand::Copy, None).as_deref(),
            Some("obsidian"),
            "the caller gets the text to put on the clipboard"
        );
    }

    #[test]
    fn an_open_dropdown_swallows_a_paste_too() {
        // The menu owns the keys; a paste behind it must not edit a filter
        // the user cannot see — the composed-text rule, for the clipboard.
        let mut ui = state();
        ui.menu = Some(super::MenuState::variants(1));
        ui.text_key(TextCommand::Paste, Some("nord"));
        assert!(ui.filter.is_empty(), "the menu owns the keys");
    }
}
