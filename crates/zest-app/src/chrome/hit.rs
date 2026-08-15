//! Where a pointer event lands in the chrome.
//!
//! The hit map is produced by the same layout pass that produces the rects,
//! which is the whole design: a region and the rectangle it was drawn as come
//! from one computation, so visuals and hit targets physically cannot drift.
//! (`docs/ROADMAP.md`, WS-A.)

use zest_proto::SessionAddr;

/// One of the window's own caption buttons, when the chrome draws them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionButton {
    Minimize,
    /// Maximise *or* restore — which one is a matter of `WindowControls`, not
    /// of a separate region, because it is one button in one place.
    Maximize,
    Close,
}

/// Which edge or corner a drag resizes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    N,
    S,
    E,
    W,
    Ne,
    Nw,
    Se,
    Sw,
}

impl From<ResizeEdge> for winit::window::ResizeDirection {
    fn from(e: ResizeEdge) -> Self {
        use winit::window::ResizeDirection as D;
        match e {
            ResizeEdge::N => D::North,
            ResizeEdge::S => D::South,
            ResizeEdge::E => D::East,
            ResizeEdge::W => D::West,
            ResizeEdge::Ne => D::NorthEast,
            ResizeEdge::Nw => D::NorthWest,
            ResizeEdge::Se => D::SouthEast,
            ResizeEdge::Sw => D::SouthWest,
        }
    }
}

impl From<ResizeEdge> for winit::window::CursorIcon {
    fn from(e: ResizeEdge) -> Self {
        winit::window::ResizeDirection::from(e).into()
    }
}

/// What a point in the chrome means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitRegion {
    /// The approval modal's panel and its full-window scrim — swallows every
    /// click that is not a button, and deliberately does not dismiss: a
    /// security prompt is answered or Esc'd, never mis-clicked away.
    ApprovalPanel,
    /// The approval modal's Approve button.
    ApprovalApprove,
    /// The approval modal's Deny button.
    ApprovalDeny,
    /// A tab; clicking activates it.
    Tab(SessionAddr),
    /// A tab's close button.
    TabClose(SessionAddr),
    /// The new-tab button.
    NewTab,
    /// Empty chrome that moves the window when dragged.
    Drag,
    /// The strip itself — catches wheel scrolling and swallows clicks that
    /// would otherwise fall through to the grid beneath.
    Strip,
    /// The title bar's "⌘K" pill; clicking opens the palette.
    PalettePill,
    /// The sidebar's search affordance; clicking opens the palette.
    SidebarSearch,
    /// The sidebar's footer ("4 hosts online · 1 asleep"); clicking opens
    /// the fleet view.
    FleetFooter,
    /// A block header's band — swallows clicks so a press on the painted-over
    /// prompt cannot select the text it hides.
    BlockHeader(u32),
    /// A block header's fold affordance; clicking folds/unfolds that block.
    BlockFold(u32),
    /// A block's rail and the gutter strip beside it, the block's full height.
    /// Clicking selects the whole block — the thing that makes a block's
    /// *extent* clickable, which the one-row header never did.
    BlockRail(u32),
    /// The `⋯` affordance in a block's header; clicking opens its menu.
    BlockMenu(u32),
    /// One row of the open block menu, by index into the model's rows.
    ///
    /// A bare index, not `(block, row)`: the open menu already knows its
    /// block, and a second copy is a second thing that can disagree — the rule
    /// `LauncherRow` and `SettingsMenuRow` already follow.
    BlockMenuRow(usize),
    /// The block menu's panel between rows — swallows, so a near-miss beside a
    /// row does not dismiss the way the scrim does.
    BlockMenuPanel,
    /// The full-window transparent region beneath the open block menu:
    /// click-away dismisses without the press reaching the grid or a header.
    BlockMenuScrim,
    /// A full-pane screen's ground (fleet, themes) — swallows what its
    /// cards do not claim; Esc is the way back.
    ScreenPanel,
    /// One pane of a split tab (`true` = right); clicking moves the keyboard
    /// there. Pushed only over the *unfocused* pane's frame and both
    /// headers — clicks inside the focused pane's body stay the grid's.
    Pane(bool),
    /// One theme card of the gallery; clicking applies that theme.
    ThemeCard(usize),
    /// One host card of the fleet view; clicking opens a fresh shell on
    /// that machine. Pushed only for cards with a live route.
    FleetCard(usize),
    /// A devices-section row's button, by row index. One region for both
    /// verbs: whether the click approves or vouches is the row's state
    /// (`FleetDeviceAction`), decided where the row was built — the same
    /// snapshot the index resolves against.
    FleetApproveDevice(usize),
    /// The fleet header's "Sign in with a code"; clicking opens the entry.
    FleetSignIn,
    /// "Sign in with browser" (#226); clicking starts the hand-off.
    FleetLinkStart,
    /// "Cancel" while the hand-off waits; clicking stops the poller.
    FleetLinkCancel,
    /// The local host card's "Enroll this machine" button (issue #227);
    /// clicking mints a code and carries it to the loopback daemon.
    FleetEnrollLocal,
    /// The fleet header's "Sign out"; clicking forgets this app's token.
    FleetSignOut,
    /// One row of the open picker, by index into its row list.
    PickerRow(usize),
    /// The picker's panel between rows (query line, group labels, footer) —
    /// swallows clicks so they cannot fall through to the grid, and so a
    /// miss beside a row does not dismiss like the scrim would.
    PickerPanel,
    /// The dimmed backdrop behind the picker; clicking it dismisses.
    PickerScrim,
    /// One runnable row of the command palette, by index; clicking runs it.
    PaletteRow(usize),
    /// The palette's panel between rows (filter line, headers, reference
    /// rows) — swallows clicks so they cannot fall through to the grid.
    PalettePanel,
    /// The dimmed backdrop behind the palette; clicking it dismisses.
    PaletteScrim,
    /// One row of the settings tab, by index; clicking selects it.
    SettingsRow(usize),
    /// A toggle's track inside its settings row; clicking flips the value.
    SettingsToggle(usize),
    /// A slider's grab band inside its settings row; click or drag sets the
    /// value from the pointer's position along the track.
    SettingsSlider(usize),
    /// The settings tab's ground (rail, headers, the space between rows) —
    /// swallows, so a click cannot fall through to the grid beneath.
    SettingsPanel,
    /// One actionable row of the + launcher menu, by index into its rows.
    LauncherRow(usize),
    /// The launcher's panel between rows (header, divider) — swallows
    /// clicks so a near-miss beside a row does not dismiss like the scrim.
    LauncherPanel,
    /// The full-window transparent region beneath the launcher panel:
    /// click-away dismisses without the press falling through to a tab,
    /// the grid, or a block header.
    LauncherScrim,
    /// One row of the settings tab's category rail; clicking selects it.
    SettingsCategory(usize),
    /// A settings row's modified dot: clicking *resets* — deletes the key
    /// from the config file (design §11, "it is the reset button"). Hittable
    /// only while the row is modified; a transparent dot takes no clicks.
    SettingsReset(usize),
    /// The rail's filter pill; typing already filters, so a click only says
    /// "yes, this is where the characters go".
    SettingsFilter,
    /// The footer's "Edit as TOML": open the config file externally.
    SettingsEditToml,
    /// One segment of a segmented select, by (row, variant index).
    SettingsSegment(usize, usize),
    /// A number stepper's − / ＋, by row; `true` steps up.
    SettingsStep(usize, bool),
    /// A long select's dropdown pill; clicking opens its menu.
    SettingsSelect(usize),
    /// One option of the open dropdown menu, by *visible* index — the model's
    /// `options` are already filtered, so this is not a variant index once a
    /// search is live.
    SettingsMenuRow(usize),
    /// The open dropdown's panel — pushed *first*, so its own rows override
    /// it where they overlap and everything else it covers (padding, the
    /// rule under the search row, the gap beside a short row) still belongs
    /// to the menu. Without it those gaps fell through to the settings pane
    /// underneath, and a wheel there scrolled the rows out from under the
    /// anchor.
    SettingsMenuPanel,
    /// The open dropdown's search row. Clicking it is a no-op that must not
    /// fall through to the scrim and dismiss the menu.
    SettingsMenuSearch,
    /// The open dropdown's footer row ("Browse all themes…").
    SettingsMenuFooter,
    /// A list item's × (font stack, tags, env entries), by (row, item).
    SettingsListRemove(usize, usize),
    /// A list widget's dashed add affordance, by row.
    SettingsListAdd(usize),
    /// A font-list item's body, by (row, item) — the drag-to-reorder handle
    /// and drop target: order IS the setting (§11).
    SettingsListItem(usize, usize),
    /// One row of the profiles editor's rail (design §12); clicking edits
    /// that profile (only the launcher launches — two different verbs).
    ProfilesRailRow(usize),
    /// The rail footer's dashed "＋ New profile".
    ProfilesNew,
    /// The editor header's Duplicate button.
    ProfilesDuplicate,
    /// The editor header's Delete button (absent on Defaults).
    ProfilesDelete,
    /// One option of a direct-choice cell in the profiles editor — a scheme
    /// swatch, an accent swatch, or an icon tile — by (row, option index).
    /// The row's cell decides what the index means; the parallel action
    /// list resolves the field, so the pair cannot drift.
    ProfilesChoice(usize, usize),
    /// A caption button we drew ourselves, on the borderless path.
    CaptionButton(CaptionButton),
    /// The window's own edge. Pushed last of everything, so it outranks even
    /// a modal scrim: a window must stay resizable while the palette is open.
    Resize(ResizeEdge),
}

/// What the wheel does where the pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelTarget {
    /// The session under the pointer: its scrollback, or the program's own
    /// wheel when it asked for mouse reporting.
    Grid,
    /// The tab strip, horizontal or as the sidebar.
    Strip,
    /// The settings tab's rows pane, or the profiles editor's.
    Settings,
    /// The open dropdown menu's own option list.
    Menu,
    /// Eaten: a surface that neither scrolls itself nor may let anything
    /// beneath it scroll.
    Swallow,
}

/// Which surface owns a wheel event that landed on `hit`.
///
/// `pane_focus_right` is `Some(focus_right)` while the active tab is split,
/// `None` otherwise — a `Pane` region means different things either side of
/// the focus, and the region alone cannot say which.
///
/// **Deliberately exhaustive — there is no `_` arm, and adding one re-opens
/// the hole this closes.** A catch-all is what made block headers, the
/// unfocused pane's frame, every full-pane screen and the approval modal the
/// *strip's*: each is a region drawn inside the grid area that nobody had
/// classified, so it fell through to "must be the strip" and the terminal
/// stopped scrolling wherever one of them lay. Without the `_`, the next
/// region added is an `error[E0004]` in this file — which is the file whose
/// enum you are already editing. (#256.)
#[must_use]
pub fn wheel_target(hit: Option<HitRegion>, pane_focus_right: Option<bool>) -> WheelTarget {
    use HitRegion as R;
    let Some(hit) = hit else {
        // Nothing in the chrome is under the pointer, so the pointer is on
        // the terminal.
        return WheelTarget::Grid;
    };
    match hit {
        // Inside the grid area, drawn over the session's own rows. Headers
        // ride the scrollback and their band spans the full grid width, so
        // this arm is most of the pane on a shell with integration loaded.
        R::BlockHeader(_) | R::BlockFold(_) | R::BlockRail(_) | R::BlockMenu(_) => {
            WheelTarget::Grid
        }
        // The focused pane's header band is that pane's; the unfocused pane's
        // *whole frame* is in the map, and the wheel's tail resolves against
        // `focused_view_rect`, so falling through there would scroll the pane
        // the pointer is not over — a different wrong answer, not a fix.
        // Routing per pane is a feature; doing nothing is the honest interim.
        R::Pane(right) => {
            if pane_focus_right == Some(right) {
                WheelTarget::Grid
            } else {
                WheelTarget::Swallow
            }
        }

        R::Strip
        | R::Tab(_)
        | R::TabClose(_)
        | R::NewTab
        | R::Drag
        | R::PalettePill
        | R::SidebarSearch
        | R::FleetFooter
        | R::CaptionButton(_) => WheelTarget::Strip,
        // The one arm that keeps today's answer on purpose. The resize bands
        // are pushed last of everything, so they overlay the strip *and* the
        // grid and the region genuinely cannot say what is beneath it — in
        // sidebar mode the W band lies over the sidebar, at the window's
        // right edge it lies over the terminal. Either choice is wrong for
        // one of them across a 5px strip; asking the map for the next region
        // underneath is the real fix, and it is not this one.
        R::Resize(_) => WheelTarget::Strip,

        R::SettingsPanel
        | R::SettingsRow(_)
        | R::SettingsToggle(_)
        | R::SettingsSlider(_)
        | R::SettingsReset(_)
        | R::SettingsCategory(_)
        | R::SettingsFilter
        | R::SettingsEditToml
        | R::SettingsSegment(..)
        | R::SettingsStep(..)
        | R::SettingsSelect(_)
        | R::SettingsListRemove(..)
        | R::SettingsListAdd(_)
        | R::SettingsListItem(..)
        | R::ProfilesChoice(..) => WheelTarget::Settings,

        // An open dropdown scrolls its own list — a roster of 266 installed
        // families does not fit in a 288px panel, and the rows underneath
        // must not move: that would slide the anchor out from under it.
        R::SettingsMenuPanel
        | R::SettingsMenuRow(_)
        | R::SettingsMenuSearch
        | R::SettingsMenuFooter => WheelTarget::Menu,
        // A modal, and a full-pane screen that covers the grid: neither has a
        // scroll of its own, and neither may let the strip scroll behind it.
        R::ApprovalPanel
        | R::ApprovalApprove
        | R::ApprovalDeny
        | R::ScreenPanel
        | R::ThemeCard(_)
        | R::FleetCard(_)
        | R::FleetApproveDevice(_)
        | R::FleetSignIn
        | R::FleetLinkStart
        | R::FleetLinkCancel
        | R::FleetEnrollLocal
        | R::FleetSignOut
        | R::ProfilesRailRow(_)
        | R::ProfilesNew
        | R::ProfilesDuplicate
        | R::ProfilesDelete => WheelTarget::Swallow,
        // Unreachable in practice — the app returns on the open-overlay state
        // before it ever consults the hit map — but the function is total,
        // and these are the right answers if that early return is ever
        // removed: an overlay that let the grid scroll beneath it would read
        // as detached from the window it floats over.
        R::PickerRow(_)
        | R::PickerPanel
        | R::PickerScrim
        | R::PaletteRow(_)
        | R::PalettePanel
        | R::PaletteScrim
        | R::LauncherRow(_)
        | R::LauncherPanel
        | R::LauncherScrim
        // The block menu's own reason to swallow is stronger than the others'
        // — its anchor is a *grid row*, so letting the grid scroll beneath it
        // would slide the block out from under its own menu.
        | R::BlockMenuRow(_)
        | R::BlockMenuPanel
        | R::BlockMenuScrim => WheelTarget::Swallow,
    }
}

/// Rectangles to meanings, in draw order.
#[derive(Debug, Default)]
pub struct ChromeHitMap {
    /// `[x, y, w, h]` in physical pixels, paired with what lives there.
    /// Push order is draw order, so lookups walk it backwards: the thing
    /// drawn last sits on top and wins.
    regions: Vec<([f32; 4], HitRegion)>,
}

impl ChromeHitMap {
    pub fn push(&mut self, rect: [f32; 4], region: HitRegion) {
        self.regions.push((rect, region));
    }

    /// The topmost region containing the point, if any.
    #[must_use]
    pub fn hit(&self, x: f32, y: f32) -> Option<HitRegion> {
        self.regions
            .iter()
            .rev()
            .find(|(r, _)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
            .map(|(_, region)| *region)
    }

    /// Test-only: the layout tests assert a chrome model always yields
    /// regions. Production code asks *where*, never *whether*.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_pushed_region_wins_where_they_overlap() {
        // A close button sits inside its tab. If the tab won, the close
        // button would be decorative.
        let mut map = ChromeHitMap::default();
        map.push([0.0, 0.0, 100.0, 30.0], HitRegion::Strip);
        map.push([80.0, 8.0, 16.0, 16.0], HitRegion::NewTab);
        assert_eq!(map.hit(85.0, 10.0), Some(HitRegion::NewTab));
        assert_eq!(map.hit(10.0, 10.0), Some(HitRegion::Strip));
        assert_eq!(map.hit(10.0, 40.0), None, "below the strip is the grid's problem");
    }

    #[test]
    fn a_block_header_leaves_the_wheel_to_the_grid() {
        // The regression test for #256. A header's band covers the grid's
        // full width and rides the scrollback, so resting on one is resting
        // on the terminal. Classified as the strip's, the wheel fed
        // `strip_scroll` — which layout re-clamps to nothing — and returned
        // before the grid: the pointer does not move during a wheel gesture,
        // so the terminal simply stopped scrolling until the mouse did.
        for region in [HitRegion::BlockHeader(7), HitRegion::BlockFold(7)] {
            assert_eq!(
                wheel_target(Some(region), None),
                WheelTarget::Grid,
                "{region:?} is drawn inside the grid area, so the wheel is the session's"
            );
        }
    }

    #[test]
    fn nothing_under_the_pointer_is_the_grids() {
        assert_eq!(
            wheel_target(None, None),
            WheelTarget::Grid,
            "no chrome under the pointer means the pointer is on the terminal"
        );
    }

    #[test]
    fn the_strip_and_the_sidebar_still_take_the_wheel() {
        // The other direction of #256: a fix that hands everything to the
        // grid would leave an overflowing tab strip unscrollable, which is
        // the feature the catch-all was written for in the first place.
        for region in [
            HitRegion::Strip,
            HitRegion::NewTab,
            HitRegion::Drag,
            HitRegion::PalettePill,
            HitRegion::SidebarSearch,
            HitRegion::FleetFooter,
            HitRegion::CaptionButton(CaptionButton::Close),
        ] {
            assert_eq!(
                wheel_target(Some(region), None),
                WheelTarget::Strip,
                "{region:?} is the strip's own furniture"
            );
        }
    }

    #[test]
    fn only_the_focused_pane_gives_the_wheel_to_the_grid() {
        // `Pane` is pushed over the focused pane's header band *and* the
        // whole frame of the unfocused one, so the region alone cannot say
        // which side it means — the focus does.
        assert_eq!(
            wheel_target(Some(HitRegion::Pane(false)), Some(false)),
            WheelTarget::Grid,
            "the focused pane's own header band belongs to that pane"
        );
        // Not Grid: the wheel path's tail resolves against
        // `focused_view_rect`, so falling through would scroll the pane the
        // pointer is *not* over. Doing nothing is the honest interim, and it
        // still beats scrolling the tab strip, which is what it did.
        assert_eq!(
            wheel_target(Some(HitRegion::Pane(true)), Some(false)),
            WheelTarget::Swallow,
            "the unfocused pane is not routed per pane yet"
        );
        assert_eq!(
            wheel_target(Some(HitRegion::Pane(true)), None),
            WheelTarget::Swallow,
            "a Pane region with no split is stale chrome; it must not reach the strip"
        );
    }

    #[test]
    fn every_settings_region_scrolls_settings() {
        // The direct successor to the hand-maintained list this replaced,
        // whose own comment recorded that "a gap in this list sent the scroll
        // to the strip or the session behind the tab".
        for region in [
            HitRegion::SettingsPanel,
            HitRegion::SettingsRow(0),
            HitRegion::SettingsToggle(0),
            HitRegion::SettingsSlider(0),
            HitRegion::SettingsReset(0),
            HitRegion::SettingsCategory(0),
            HitRegion::SettingsFilter,
            HitRegion::SettingsEditToml,
            HitRegion::SettingsSegment(0, 0),
            HitRegion::SettingsStep(0, true),
            HitRegion::SettingsSelect(0),
            HitRegion::SettingsListRemove(0, 0),
            HitRegion::SettingsListAdd(0),
            HitRegion::SettingsListItem(0, 0),
            HitRegion::ProfilesChoice(0, 0),
        ] {
            assert_eq!(
                wheel_target(Some(region), None),
                WheelTarget::Settings,
                "{region:?} is the settings pane's"
            );
        }
        // The menu scrolls its own list since #259 — it grew one, because a
        // 266-family roster does not fit in a 288px panel. What has not
        // changed is that the rows *underneath* must not move: that would
        // slide the anchor out from under the menu.
        for region in [
            HitRegion::SettingsMenuPanel,
            HitRegion::SettingsMenuRow(0),
            HitRegion::SettingsMenuSearch,
            HitRegion::SettingsMenuFooter,
        ] {
            assert_eq!(
                wheel_target(Some(region), None),
                WheelTarget::Menu,
                "{region:?} scrolls the open menu, never the pane behind it"
            );
        }
    }

    #[test]
    fn a_surface_that_covers_the_grid_swallows_the_wheel() {
        // The same defect as the block header's, in three places nobody had
        // reported: a screen that covers the grid has no scroll of its own,
        // so classifying it as the strip's scrolled the tab strip *behind* it.
        for region in [
            HitRegion::ScreenPanel,
            HitRegion::FleetCard(0),
            HitRegion::ThemeCard(0),
            HitRegion::ApprovalPanel,
        ] {
            assert_eq!(
                wheel_target(Some(region), None),
                WheelTarget::Swallow,
                "{region:?} covers the grid and scrolls nothing"
            );
        }
    }

    #[test]
    fn edges_are_half_open() {
        // [x, x+w): a point exactly on the right edge belongs to whatever is
        // next, or two adjacent tabs would both claim their shared border.
        let mut map = ChromeHitMap::default();
        map.push([0.0, 0.0, 10.0, 10.0], HitRegion::Drag);
        assert_eq!(map.hit(0.0, 0.0), Some(HitRegion::Drag));
        assert_eq!(map.hit(10.0, 5.0), None);
    }
}
