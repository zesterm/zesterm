//! The Profiles tab: the §12 editor behind a tab of its own.
//!
//! The 25 `profiles_*` methods, moved out of `app/mod.rs` (#554). `App`'s
//! `profiles_ui` field and `ProfilesUiState` itself stay on the parent, which
//! reads them at 32 sites outside this module -- the same rule `gpu.rs`
//! follows, and the reason the split costs no visibility change beyond the
//! `pub(super)` that sharing one file used to give these methods implicitly.
//!
//! The rule these carry, and the one worth not undoing (#272): **leaving a
//! field is a commit, and only Esc discards.** It lives in
//! `take_pending_edit`, which every exit routes through, rather than in the
//! Enter arm -- an edit whose only commit is Enter loses work through every
//! other way out, and each of those exits then looks like a different bug.

use super::*;

impl App {
    /// The field index a profiles row stands for, when it is a real field.
    pub(super) fn profiles_field_of_row(&self, row: usize) -> Option<usize> {
        match self.profiles_ui.as_ref()?.actions.get(row) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }

    /// The selected profiles row's field index, when it is a real field.
    fn profiles_selected_field(&self) -> Option<usize> {
        self.profiles_field_of_row(self.profiles_ui.as_ref()?.selected)
    }

    /// The value a profiles row currently SHOWS — the profile's resolved
    /// value, or the window's where the profile is silent — so an arrow
    /// press or slider drag steps from what is on screen, exactly like the
    /// Settings tab. Not the typed-edit seed: that is [`Self::profiles_seed_of`].
    pub(super) fn profiles_value_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        self.profiles_eval(field_idx, crate::profiles_ui::effective_value)
    }

    /// The value a typed edit opens with — the launch strings seed the
    /// profile's own resolved value (empty when unset), never the display
    /// fallback: `effective_value` captions an unset `command` with
    /// `shell_fallback()` (on a remote route, "the host's default shell")
    /// and an unset `host` with a label no fleet entry carries, and seeding
    /// either puts two Enters between the caption and a real
    /// `[profiles.<name>]` value a launch would spawn verbatim.
    pub(super) fn profiles_seed_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        self.profiles_eval(field_idx, crate::profiles_ui::edit_seed_value)
    }

    /// Defaults' own env entries, as the object `env_value_replacing` wants.
    ///
    /// `own_env` *on the Defaults profile* is the Defaults environment, since
    /// `resolve_profile` gives that one an empty parent on purpose -- so this
    /// needs no new resolver field.
    ///
    /// Empty when the profile being edited is Defaults itself. There is no
    /// layer under it for a renamed-away key to survive in, and claiming
    /// otherwise would write a tombstone for a variable nothing sets.
    pub(super) fn profiles_defaults_env(&self) -> serde_json::Value {
        use zest_config::profiles::RESERVED_PROFILE;
        let editing_defaults =
            self.profiles_ui.as_ref().is_some_and(|ui| ui.profile == RESERVED_PROFILE);
        if editing_defaults {
            return serde_json::Value::Object(serde_json::Map::new());
        }
        let root = crate::launcher::profiles_root(&self.settings);
        let defaults = zest_config::profiles::resolve_profile(&root, RESERVED_PROFILE);
        serde_json::Value::Object(
            defaults
                .own_env
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        )
    }

    fn profiles_eval(
        &self,
        field_idx: usize,
        eval: fn(
            &zest_config::ui::UiField,
            &zest_config::profiles::ProfileResolved,
            &serde_json::Value,
            &crate::profiles_ui::ProfileRowContext,
        ) -> serde_json::Value,
    ) -> Option<serde_json::Value> {
        use crate::profiles_ui as pui;
        let ui = self.profiles_ui.as_ref()?;
        let field = ui.fields.get(field_idx)?;
        let root = crate::launcher::profiles_root(&self.settings);
        let resolved = zest_config::profiles::resolve_profile(&root, &ui.profile);
        let overrides = pui::overrides_json(&resolved);
        let window_values = serde_json::to_value(&self.settings).ok()?;
        let fallback = self.shell_fallback();
        // hosts/schemes are display-only inputs neither evaluator reads
        // (their docs pin that), so the input path skips the snapshot. The
        // placeholder `local_host` is likewise never written: the launch
        // strings it captions refuse arrow-adjust (`adjust_profile` returns
        // `None`) and seed through `edit_seed_value`, which ignores it.
        let ctx = pui::ProfileRowContext {
            window_values: &window_values,
            window_theme: theme_id(&self.config, self.system_light),
            fallback_command: &fallback,
            local_host: "this machine",
            hosts: &[],
            schemes: &[],
            is_defaults: ui.profile == zest_config::profiles::RESERVED_PROFILE,
        };
        Some(eval(field, &resolved, &overrides, &ctx))
    }

    /// Write one edited value into `[profiles.<name>]`, then reload — never
    /// the root file: the root is the window's, the table is the profile's.
    pub(super) fn profiles_apply_edit(&mut self, field_idx: usize, new_value: serde_json::Value) {
        let Some((profile, key, value)) = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(field_idx)?;
            Some((
                ui.profile.clone(),
                field.key.clone(),
                crate::settings_ui::to_toml(field, &new_value)?,
            ))
        }) else {
            // Unreachable while every widget `fields()` emits has a `to_toml`
            // arm — which is exactly why it must not be a bare `return`. The
            // next widget added without one would otherwise be a control that
            // silently does nothing (#272).
            self.profiles_report(format!("this field cannot be written (field {field_idx})"));
            return;
        };
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        match zest_config::write_profile_value(&target, &profile, &key, value) {
            Ok(()) => {
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = None;
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, profile = %profile, "could not write the profile value");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not save {key}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// The override dot's click: delete the key from `[profiles.<name>]`,
    /// reload — the row falls back through Defaults (§12). Idempotent
    /// because `remove_profile_value` is.
    pub(super) fn profiles_reset_row(&mut self, row: usize) {
        let Some((profile, key)) = self.profiles_field_of_row(row).and_then(|i| {
            let ui = self.profiles_ui.as_ref()?;
            Some((ui.profile.clone(), ui.fields.get(i)?.key.clone()))
        }) else {
            return;
        };
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        match zest_config::remove_profile_value(&target, &profile, &key) {
            Ok(()) => {
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = None;
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, profile = %profile, "could not clear the override");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not reset {key}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Arrow-key editing on the selected profiles row: flip, cycle or step,
    /// then write through the profile path.
    pub(super) fn profiles_adjust(&mut self, dir: i32) {
        let Some(idx) = self.profiles_selected_field() else { return };
        let Some(current) = self.profiles_value_of(idx) else { return };
        let installed: Vec<String> = self
            .profiles_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .filter(|f| f.widget == zest_config::ui::Widget::FontList)
            .and(self.fonts.as_mut())
            .map(Fonts::installed_families)
            .unwrap_or_default();
        let next = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            let themes: Vec<String> = crate::themes::ids();
            crate::profiles_ui::adjust_profile(field, &current, dir, &themes, &installed)
        });
        if let Some(value) = next {
            self.profiles_apply_edit(idx, value);
        } else {
            // A row its widget cannot step (HostPicker). Repaint anyway, or
            // the arrow key is a no-op that does not even move the selection
            // highlight — indistinguishable from a dead keyboard (#272).
            self.mark_chrome_dirty();
        }
    }

    /// Enter on the selected profiles row: act the way its widget wants.
    pub(super) fn profiles_activate_selected(&mut self) {
        use zest_config::ui::Widget;
        let Some(idx) = self.profiles_selected_field() else { return };
        let Some((widget, key, segmented)) =
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| {
                (f.widget, f.key.clone(), crate::settings_ui::select_is_segmented(f))
            })
        else {
            return;
        };
        match widget {
            Widget::Toggle => self.profiles_adjust(1),
            // tab_title's third state is typing (the #135 contract: any
            // other string is a custom title) — Enter opens a buffer; the
            // drawn segments stay the two fixed spellings.
            Widget::Select if key == "tab_title" => self.profiles_begin_edit(idx),
            Widget::Select if segmented => self.profiles_adjust(1),
            Widget::Select => {
                let row = self.profiles_ui.as_ref().map(|ui| ui.selected);
                if let (Some(ui), Some(row)) = (self.profiles_ui.as_mut(), row) {
                    ui.menu = Some(MenuState::variants(row));
                }
            }
            // The ▾ opens the fleet (#297). It promised a dropdown and gave a
            // text field, which is also how a `host` key the fleet has never
            // heard of gets written — the pin #268 has to render as its own
            // "not in the fleet" group. Falling back to typing when there is
            // no roster keeps the field editable on a machine that has not
            // discovered anything yet.
            Widget::HostPicker => {
                if !self.profiles_open_host_menu() {
                    self.profiles_begin_edit(idx);
                }
            }
            Widget::Number | Widget::Slider | Widget::Text | Widget::Path | Widget::FilePath => {
                self.profiles_begin_edit(idx);
            }
            // The rosters open the Settings tab's dropdown — a ▾ pill should
            // open, not cycle. Falling back to a step keeps the keypress
            // meaning something when there is nothing to list.
            Widget::FontList | Widget::ThemePicker => {
                if !self.profiles_open_roster_menu() {
                    self.profiles_adjust(1);
                }
            }
            // The direct-choice rows answer Enter by stepping, so the
            // keyboard can drive them without a pointer. These are swatch and
            // tile rows (§12) — the choices are already all on screen, and a
            // dropdown over them would hide what it is choosing between.
            Widget::SchemePicker | Widget::AccentPicker | Widget::IconPicker => {
                self.profiles_adjust(1);
            }
            // The add affordance was pointer-only here while the Settings tab
            // reached it from the keyboard (#550) -- and selection plus Enter
            // is the path that never goes near a hit region at all.
            Widget::TagList | Widget::KeyValue => {
                if let Some(row) = self.profiles_ui.as_ref().map(|ui| ui.selected) {
                    self.begin_list_add(row);
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// The Settings tab's [`Self::open_roster_menu`], on §12's surface.
    /// The host `▾`'s dropdown: this machine, then the fleet (#297).
    ///
    /// **`ask_host` is deliberately not in here.** §12 gives it its own *"Ask
    /// which host at launch"* toggle, which the editor already renders, and a
    /// menu of machines that silently flipped a different row would be the
    /// surprising thing — "any machine" is not a machine.
    ///
    /// The roster is the fleet snapshot, which is discovery ∪ the account. So
    /// a machine that is genuinely yours is listed whether or not this network
    /// can see it right now, and a label in neither is one #268 already draws
    /// as "not in the fleet".
    ///
    /// **The current value is always in the list**, even when the fleet has
    /// never heard of it. A profile hand-written for a machine that is off must
    /// not become uneditable, and must not lose its pin to whatever the menu
    /// happened to open on.
    fn profiles_open_host_menu(&mut self) -> bool {
        let Some(idx) = self.profiles_selected_field() else { return false };
        // The profile's *own* value, not the resolved one — `profiles_seed_of`'s
        // rule: the ✓ marks what this profile sets, never what it inherits.
        let current = self.profiles_seed_of(idx).and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        });
        // Every rule about what the dropdown lists, and whether there should be
        // one at all, lives in `host_menu_roster` — pure, and tested.
        let Some(roster) = host_menu_roster(&self.fleet_view, current.as_deref()) else {
            return false;
        };
        let selected = host_menu_selection(&roster, current.as_deref());
        let row = self.profiles_ui.as_ref().map(|ui| ui.selected).unwrap_or(0);
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.menu = Some(MenuState::roster(row, roster, Some(&selected)));
        }
        self.mark_chrome_dirty();
        true
    }

    fn profiles_open_roster_menu(&mut self) -> bool {
        let Some(idx) = self.profiles_selected_field() else { return false };
        let Some(field) = self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)) else {
            return false;
        };
        let roster = match field.widget {
            zest_config::ui::Widget::FontList => {
                self.fonts.as_mut().map(Fonts::installed_families).unwrap_or_default()
            }
            zest_config::ui::Widget::ThemePicker => crate::themes::ids(),
            _ => return false,
        };
        if roster.is_empty() {
            return false;
        }
        // The profile's *own* value, not the resolved one — the seeding rule
        // `profiles_seed_of` documents: the ✓ marks what this profile sets,
        // not what it inherits.
        let current = self.profiles_seed_of(idx).and_then(|v| match &v {
            serde_json::Value::Array(a) => a.first().and_then(|f| f.as_str().map(str::to_string)),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let row = self.profiles_ui.as_ref().map(|ui| ui.selected).unwrap_or(0);
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.menu = Some(MenuState::roster(row, roster, current.as_deref()));
        }
        self.mark_chrome_dirty();
        true
    }

    /// Write the profiles dropdown's chosen option — `apply_menu_choice`,
    /// through §12's write path.
    pub(super) fn profiles_apply_menu_choice(&mut self, opt: usize) {
        self.mark_chrome_dirty();
        // Resolved before the menu is taken, for the Settings tab's reason:
        // Enter on a search that matched nothing must leave the dropdown
        // alone rather than close it having applied nothing.
        let Some((row, chosen)) = self.profiles_ui.as_ref().and_then(|ui| {
            let menu = ui.menu.as_ref()?;
            if menu.roster.is_empty() {
                return Some((menu.row, None));
            }
            Some((menu.row, Some(menu.matching().get(opt)?.clone())))
        }) else {
            return;
        };
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.menu = None;
        }
        let Some(chosen) = chosen else {
            self.profiles_apply_variant(row, opt);
            return;
        };
        let Some(idx) = self.profiles_field_of_row(row) else { return };
        let Some(widget) =
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return;
        };
        let value = match widget {
            zest_config::ui::Widget::FontList => {
                serde_json::Value::Array(vec![serde_json::Value::String(chosen)])
            }
            // "(this machine)" is the menu's spelling of unset, and the file's
            // is an empty string — the same one `launch::resolve_host` reads as
            // Local and `bucket_for` files under "this machine". Writing the
            // label back would put a display name in the config where a
            // machine-independent "no pin" belongs.
            zest_config::ui::Widget::HostPicker if chosen == HOST_MENU_LOCAL => {
                serde_json::Value::String(String::new())
            }
            _ => serde_json::Value::String(chosen),
        };
        self.profiles_apply_edit(idx, value);
    }

    /// Say why nothing happened.
    ///
    /// Every profiles mutation has a bail-out that writes nothing, and one
    /// that also says nothing is indistinguishable from a save that worked —
    /// which is how a lost edit reads as a saved one (#272).
    pub(super) fn profiles_report(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::error!(reason = %message, "the profiles editor wrote nothing");
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.error = Some(message);
        }
        self.mark_chrome_dirty();
    }

    /// Commit an open profiles edit, if there is one.
    ///
    /// `true` means the caller may proceed. `false` means a buffer is open and
    /// does not parse: it stays on screen with its warn border and the caller
    /// must NOT move — leaving would destroy it, which is the whole of #272.
    pub(super) fn profiles_commit_edit(&mut self) -> bool {
        // The name entry leaves by the same rule as every field (#272/#283):
        // one exit, one meaning. A name that will not validate keeps the entry
        // open and blocks the exit, exactly as an unparseable value does.
        if self.profiles_ui.as_ref().is_some_and(|ui| ui.renaming.is_some()) {
            self.profiles_commit_rename();
            // Still open means it was refused, so the caller must not leave.
            return self.profiles_ui.as_ref().is_none_or(|ui| ui.renaming.is_none());
        }
        let pending = match self.profiles_ui.as_mut() {
            Some(ui) => ui.take_pending_edit(),
            None => return true,
        };
        match pending {
            crate::settings_ui::Pending::None => true,
            crate::settings_ui::Pending::Refused => {
                self.mark_chrome_dirty();
                false
            }
            crate::settings_ui::Pending::Commit(idx, value) => {
                self.profiles_apply_edit(idx, value);
                true
            }
            // Reachable since §12 grew `env` (#496): `begin_list_add` opens
            // an append buffer on this tab too, and its Enter lands here.
            // Kept as an explicit arm rather than folded in, because the
            // *reason* it was unreachable — no profiles field was a list —
            // stopped being true, and a swallowed Enter is #272 again.
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

    /// Click the header name: open the rename entry, seeded and selected so
    /// typing replaces the old name (#283).
    ///
    /// Defaults is refused here as well as undrawn — the screen pushes no
    /// region for it, and a second guard costs nothing next to a cascade with
    /// two parents.
    pub(super) fn profiles_begin_rename(&mut self) {
        // Clicking the name while it is already the name entry is not a new
        // edit; re-seeding would throw away what has been typed.
        if self.profiles_ui.as_ref().is_some_and(|ui| ui.renaming.is_some()) {
            return;
        }
        // Opening the name entry is leaving whatever field was open, so it
        // commits like any other exit (#272).
        if !self.profiles_commit_edit() {
            return;
        }
        let Some(ui) = self.profiles_ui.as_mut() else { return };
        if ui.profile == zest_config::profiles::RESERVED_PROFILE {
            return;
        }
        let mut buffer = TextField::new(ui.profile.clone());
        buffer.select_all();
        ui.renaming = Some(buffer);
        ui.rename_error = None;
        self.mark_chrome_dirty();
    }

    /// Commit the rename entry: validate, write, reload, and carry the editor
    /// and every open tab to the new name.
    ///
    /// A name that cannot be used keeps the entry open with the reason under
    /// it — the same rule the field edits follow, and for the same reason: an
    /// entry that closes on a refusal has destroyed what it refused.
    pub(super) fn profiles_commit_rename(&mut self) {
        let Some((from, to)) = self.profiles_ui.as_ref().and_then(|ui| {
            let typed = ui.renaming.as_ref()?.text().trim().to_string();
            Some((ui.profile.clone(), typed))
        }) else {
            return;
        };
        let names = crate::profiles_ui::rail_names(&self.settings);
        if let Some(why) = crate::profiles_ui::rename_error(&names, &from, &to) {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.rename_error = Some(why.to_string());
            }
            self.mark_chrome_dirty();
            return;
        }
        if from == to {
            self.profiles_cancel_rename();
            return;
        }
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        match zest_config::rename_profile(&target, &from, &to) {
            Ok(()) => {
                // Before the reload: `ProfileIdentity` re-resolves by name, and
                // a name that no longer exists resolves as empty-over-Defaults
                // *silently* (`tabs.rs`), so a tab left pointing at the old one
                // would quietly lose its scheme, accent and icon with nothing
                // to see. The tabs are renamed with the profile, not by it.
                self.tabs.rename_profile(&from, &to);
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = to;
                    ui.renaming = None;
                    ui.rename_error = None;
                    ui.scroll_to_selected = true;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %from, "could not rename the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    // On the entry rather than the banner: the name is what
                    // failed, and that is where the user is looking.
                    ui.rename_error = Some(format!("could not rename: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Esc on the name entry: close it, keeping the profile's real name.
    pub(super) fn profiles_cancel_rename(&mut self) {
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.renaming = None;
            ui.rename_error = None;
        }
        self.mark_chrome_dirty();
    }

    /// Open a typed edit on a profiles field, seeded with the profile's own
    /// value — see `profiles_seed_of` for why not with what the row shows.
    fn profiles_begin_edit(&mut self, idx: usize) {
        // Moving from one field to another is leaving the first one, so it
        // commits like any other exit (#272).
        if !self.profiles_commit_edit() {
            return;
        }
        let current = self.profiles_seed_of(idx);
        let seed = match &current {
            // Strings seed verbatim whatever the widget (host, tab_title,
            // command); numbers go through the settings seeding.
            Some(serde_json::Value::String(s)) => s.clone(),
            other => self
                .profiles_ui
                .as_ref()
                .and_then(|ui| ui.fields.get(idx))
                .map(|f| crate::settings_ui::edit_seed(f, other.as_ref()))
                .unwrap_or_default(),
        };
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.editing = Some(crate::settings_ui::EditBuffer {
                field_idx: idx,
                buffer: TextField::new(seed),
                error: false,
                list: crate::settings_ui::ListEdit::Value,
            });
        }
        self.mark_chrome_dirty();
    }

    /// A direct-choice click (scheme swatch, accent swatch, icon tile).
    pub(super) fn profiles_choice(&mut self, row: usize, opt: usize) {
        // Clicking a swatch is leaving whatever field was open (#272).
        if !self.profiles_commit_edit() {
            return;
        }
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.selected = row;
        }
        let Some(idx) = self.profiles_field_of_row(row) else { return };
        let value = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            crate::profiles_ui::choice_value(field, opt, &crate::profiles_ui::scheme_swatches())
        });
        if let Some(value) = value {
            self.profiles_apply_edit(idx, value);
        } else {
            self.mark_chrome_dirty();
        }
    }

    /// Write one of a select field's variants — profiles-side twin of
    /// `apply_variant`, landing in the profile's table.
    pub(super) fn profiles_apply_variant(&mut self, row: usize, opt: usize) {
        let Some((idx, value)) = self.profiles_field_of_row(row).and_then(|i| {
            let field = self.profiles_ui.as_ref()?.fields.get(i)?;
            let variant = field.variants.get(opt)?;
            Some((i, serde_json::Value::String(variant.value.clone())))
        }) else {
            return;
        };
        self.profiles_apply_edit(idx, value);
    }

    /// Select the rail's `i`-th profile for editing; selection and scroll
    /// reset — a profile is a fresh page, not a continuation.
    pub(super) fn profiles_select_rail(&mut self, i: usize) {
        let names = crate::profiles_ui::rail_names(&self.settings);
        let Some(name) = names.get(i).cloned() else { return };
        let moving = self.profiles_ui.as_ref().is_some_and(|ui| ui.profile != name);
        // Commit before moving, and stay put if the buffer cannot be written:
        // switching profile used to drop it silently (#272).
        if moving && !self.profiles_commit_edit() {
            return;
        }
        if let Some(ui) = self.profiles_ui.as_mut() {
            if ui.profile != name {
                ui.profile = name;
                ui.selected = 0;
                ui.scroll = 0.0;
                ui.scroll_to_selected = true;
                ui.menu = None;
                ui.error = None;
            }
        }
        self.mark_chrome_dirty();
    }

    /// "＋ New profile": create `[profiles.new-profile-N]` (unique), reload,
    /// select it.
    pub(super) fn profiles_new(&mut self) {
        // The new profile is selected below, so this leaves the current one
        // and commits like any other exit (#272).
        if !self.profiles_commit_edit() {
            return;
        }
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        let names = crate::profiles_ui::rail_names(&self.settings);
        let name = crate::profiles_ui::new_profile_name(&names);
        match zest_config::create_profile(&target, &name) {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = name;
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    ui.scroll_to_selected = true;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %name, "could not create the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not create {name}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Duplicate the edited profile under a unique sibling name and select
    /// the copy.
    ///
    /// Renaming used to ride this plus Delete — §12 offered either shape and
    /// that one kept the header name read-only. It cost more than it saved:
    /// the copy is a different key, so every open tab launched from the
    /// original silently degraded to Defaults and the ⌘1–9 ordering moved
    /// underneath the user. `profiles_begin_rename` renames in place (#283);
    /// Duplicate is now only ever a copy.
    pub(super) fn profiles_duplicate(&mut self) {
        // Before the copy is taken, or the open buffer would ride into it and
        // a later Enter would write it to the copy instead of to the profile
        // it was typed for (#272).
        if !self.profiles_commit_edit() {
            return;
        }
        let Some(from) = self.profiles_ui.as_ref().map(|ui| ui.profile.clone()) else { return };
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        let names = crate::profiles_ui::rail_names(&self.settings);
        let to = crate::profiles_ui::copy_name(&names, &from);
        // Defaults (or a layer-supplied profile) may have no table in the
        // user's file to copy — an empty duplicate still falls through
        // Defaults, which is exactly what a copy of it means.
        let result = zest_config::copy_profile(&target, &from, &to).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                zest_config::create_profile(&target, &to)
            } else {
                Err(e)
            }
        });
        match result {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = to;
                    ui.scroll_to_selected = true;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %from, "could not duplicate the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not duplicate {from}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Delete the edited profile; the editor falls back to Defaults. The
    /// screen never draws Delete for Defaults, and this guards it anyway.
    pub(super) fn profiles_delete(&mut self) {
        let Some(name) = self.profiles_ui.as_ref().map(|ui| ui.profile.clone()) else { return };
        if name == zest_config::profiles::RESERVED_PROFILE {
            return;
        }
        let Some(target) = Self::config_target() else {
            self.profiles_report("no config directory on this system");
            return;
        };
        match zest_config::remove_profile(&target, &name) {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    // The one exit that discards rather than commits, and the
                    // only one besides Esc: the table the buffer would be
                    // written into has just been removed, so committing it is
                    // a write with nowhere to land — and leaving it open would
                    // carry it into Defaults on the next Enter, which is the
                    // misplaced-write half of #272. Only on the success arm:
                    // every path that deletes nothing must cost nothing.
                    ui.editing = None;
                    ui.renaming = None;
                    ui.rename_error = None;
                    ui.profile = zest_config::profiles::RESERVED_PROFILE.to_string();
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %name, "could not delete the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not delete {name}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }
}

#[cfg(test)]
mod profiles_edit_tests {
    use super::{ProfilesUiState, TextCommand, TextField};

    fn state() -> ProfilesUiState {
        ProfilesUiState {
            profile: "wsl-2".into(),
            selected: 0,
            filter: TextField::default(),
            scroll: 0.0,
            scroll_to_selected: false,
            actions: Vec::new(),
            fields: zest_config::profiles::fields(),
            editing: None,
            renaming: None,
            rename_error: None,
            menu: None,
            error: None,
        }
    }

    fn command_field(ui: &ProfilesUiState) -> usize {
        ui.fields.iter().position(|f| f.key == "command").expect("command is a field")
    }

    fn open(ui: &mut ProfilesUiState, field_idx: usize, text: &str) {
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx,
            buffer: TextField::new(text),
            error: false,
            list: crate::settings_ui::ListEdit::Value,
        });
    }

    #[test]
    fn an_append_on_the_env_row_hands_back_an_append_not_a_commit() {
        // The half of #496's editor gap this level can see. `begin_list_add`
        // opens a buffer with `append` set on the profiles tab now, and the
        // commit path's `Pending::Append` arm used to answer "this field
        // cannot be appended to" — the row drew an add chip that did nothing.
        //
        // Asserting the *variant* is the point: a `Commit` here would write
        // the typed text as the whole env table rather than adding one entry,
        // which is a worse outcome than the no-op it replaced.
        let mut ui = state();
        let idx = ui.fields.iter().position(|f| f.key == "env").expect("env is a field");
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx: idx,
            buffer: TextField::new("CLAUDE_CONFIG_DIR=${profile_dir}/claude"),
            error: false,
            list: crate::settings_ui::ListEdit::Append,
        });
        assert_eq!(
            ui.take_pending_edit(),
            crate::settings_ui::Pending::Append(
                idx,
                "CLAUDE_CONFIG_DIR=${profile_dir}/claude".into(),
            ),
            "an append buffer must reach the append arm, or the add chip is decoration"
        );
    }

    #[test]
    fn a_pasted_command_survives_leaving_the_field() {
        // The reported bug (#272): paste a command path, then click another
        // profile or close the tab, and the text was gone. Every exit but
        // Enter cleared `editing` outright, so the value never reached
        // `profiles_apply_edit` at all — and the buffer that held it was the
        // only copy.
        let mut ui = state();
        let idx = command_field(&ui);
        open(&mut ui, idx, "/opt/homebrew/bin/fish");
        assert_eq!(
            ui.take_pending_edit(),
            crate::settings_ui::Pending::Commit(
                idx,
                serde_json::Value::String("/opt/homebrew/bin/fish".into()),
            ),
            "leaving the field must hand the pasted path back to be written"
        );
        assert!(ui.editing.is_none(), "and close the buffer, exactly as Enter does");
    }

    #[test]
    fn a_paste_reaches_the_buffer_and_not_the_filter() {
        // The other half of the same report: the text DID get in, which is
        // what placed the bug after the keystroke rather than in it. If this
        // ever fails the symptom is identical and the cause is not.
        let mut ui = state();
        let idx = command_field(&ui);
        open(&mut ui, idx, "");
        ui.text_key(TextCommand::Paste, Some("/opt/homebrew/bin/fish"));
        let edit = ui.editing.as_ref().expect("still editing");
        assert_eq!(
            edit.buffer.text(),
            "/opt/homebrew/bin/fish",
            "the clipboard landed in the open buffer"
        );
        assert!(ui.filter.is_empty(), "and nothing leaked into the filter");
    }
}
