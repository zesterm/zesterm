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
    /// The hover chip that copies the last command's output.
    BlockCopy(u32),
    /// The hover chip that re-runs the last command.
    BlockRerun(u32),
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
    /// One option of the open dropdown menu, by variant index.
    SettingsMenuRow(usize),
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
    fn edges_are_half_open() {
        // [x, x+w): a point exactly on the right edge belongs to whatever is
        // next, or two adjacent tabs would both claim their shared border.
        let mut map = ChromeHitMap::default();
        map.push([0.0, 0.0, 10.0, 10.0], HitRegion::Drag);
        assert_eq!(map.hit(0.0, 0.0), Some(HitRegion::Drag));
        assert_eq!(map.hit(10.0, 5.0), None);
    }
}
