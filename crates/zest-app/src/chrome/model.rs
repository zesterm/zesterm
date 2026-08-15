//! What the chrome shows, as data.
//!
//! The model is built by the app from live state (tabs, roster, hover) and
//! consumed by `layout`, which is pure. Nothing here knows about the GPU, the
//! window, or the network — that is what makes the layout tests meaningful.

use zest_config::settings::TabsPosition;
use zest_config::ColorFrom;
use zest_proto::SessionAddr;

use super::hit::HitRegion;
use crate::tabs::ProfileIdentity;

/// Where a tab chip's accent colour — the 2px rule and the glyph tile, the
/// chrome's whole per-tab concession (design §12) — comes from.
///
/// A choice rather than a finished colour so the model stays theme-free:
/// layout resolves it against `ChromeColors`, and a theme change repaints
/// without rebuilding the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccentChoice {
    /// The profile's own colour: an index into the theme's accent row.
    Profile(u8),
    /// A slot in the host-accent cycle (slot 0 is the local machine).
    Host(usize),
}

/// Which accent a tab draws, per §12: the profile's own `tab_color` unless
/// `color_from` says the host decides — and the host also decides when the
/// profile never picked a colour, or the tab has no profile at all.
#[must_use]
pub fn tab_accent(identity: Option<&ProfileIdentity>, host_slot: usize) -> AccentChoice {
    match identity {
        Some(id) => match (id.color_from, id.tab_color) {
            (Some(ColorFrom::Host), _) | (_, None) => AccentChoice::Host(host_slot),
            (_, Some(color)) => AccentChoice::Profile(color),
        },
        None => AccentChoice::Host(host_slot),
    }
}

/// How reachable a tab's host currently looks.
///
/// A projection of `zest_mesh::Presence` so the chrome does not grow a mesh
/// dependency for four names. `Unreachable` is the one that changes drawing:
/// the tab stays put and says so, because a session on a sleeping laptop is
/// not gone (#22, #23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPresence {
    Online,
    Away,
    Unseen,
    Unreachable,
}

/// Which machine a tab's shell runs on, as the chrome should say it.
///
/// Origin is displayed with *text*, not colour alone — the class of mistake
/// this UI exists to prevent is acting on the wrong machine, and colour is
/// the first thing a theme change or colour-blindness takes away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabOrigin {
    Local,
    Remote { host_label: String },
}

/// What kind of thing a tab is: a session, or one of the app's own screens.
///
/// App tabs (design §11) are ordinary tabs in the strip — same geometry, same
/// active treatment — but they size to their content, carry no close
/// affordance and no accent wash: they are places, not shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Session,
    Settings,
    Profiles,
}

/// One tab, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct TabModel {
    pub addr: SessionAddr,
    pub kind: TabKind,
    /// Already derived: OSC title, else cwd basename, else "shell".
    pub title: String,
    /// The machine's display name ("studio"). Layout composes the chip's
    /// `host · cwd` line and the sidebar's grouping from this.
    pub host: String,
    /// Working directory, home-shortened for local sessions only.
    pub cwd: String,
    pub origin: TabOrigin,
    pub presence: TabPresence,
    /// Which slot of the host-accent cycle this tab's machine draws in.
    /// Slot 0 is always the local machine; remotes take the next slots in
    /// first-seen order, so a host keeps its colour while the window lives.
    /// The sidebar's host grouping and the pane headers read this; the chip
    /// itself draws `tab_accent`.
    pub accent: usize,
    /// The chip's resolved accent choice — [`tab_accent`]'s answer for this
    /// tab's identity and host slot. The 2px rule and the glyph tile take it.
    pub tab_accent: AccentChoice,
    /// A command is currently running in this session — the sidebar's
    /// pulsing dot.
    pub running: bool,
    /// How long since this session last produced output, pre-formatted
    /// ("2m", "12h"); empty when unknown.
    pub age: String,
    /// An attach or restore is in flight; the tab shows itself but cannot be
    /// typed into yet.
    pub connecting: bool,
    /// How this tab's host is currently reached. The one fact the deleted
    /// status bar owned alone was link degradation, and it surfaces here:
    /// the chip's glyph tile takes warn ink when stalled, danger when
    /// reconnecting.
    pub link: LinkKind,
}

impl TabModel {
    /// `host · cwd`, or just the host when the cwd is unknown — the vertical
    /// header's identity line. (The horizontal chip deliberately does not
    /// draw this: a second line on a 34px chip was unreadable at 9.5px.)
    #[must_use]
    pub fn detail(&self) -> String {
        if self.cwd.is_empty() {
            self.host.clone()
        } else {
            format!("{} · {}", self.host, self.cwd)
        }
    }
}

/// One host group of the sidebar (design screen 2).
#[derive(Debug, Clone, PartialEq)]
pub struct HostGroup {
    pub label: String,
    /// Host-accent slot, matching the tabs it groups.
    pub accent: usize,
    /// Mono sub-label — path and latency ("LAN 0.4 ms"); empty when unknown.
    pub sub: String,
    pub online: bool,
    /// Indices into `ChromeModel::tabs`, in strip order.
    pub tabs: Vec<usize>,
}

/// What the vertical sidebar shows beyond the tabs themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarModel {
    pub groups: Vec<HostGroup>,
    /// Fleet-wide counts for the footer — every known host, tabbed or not.
    pub hosts_online: usize,
    pub hosts_asleep: usize,
}

/// One row of the ⌘K palette (design screen 6), ready to draw.
///
/// Display-only: the app keeps a parallel list of *actions* built in the
/// same pass, so row index `n` here and there mean the same thing by
/// construction — the drift the hit map exists to prevent, applied to rows.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRow {
    /// A group label: Blocks, Sessions, Hosts, Actions — in that order,
    /// blocks first, because the palette is primarily a history of what ran
    /// anywhere in the fleet.
    Group { title: String },
    /// A command from that history. `ok` colours the ↺ glyph.
    Block { command: String, provenance: String, ok: bool },
    /// A session somewhere in the fleet. `host` is the right-aligned
    /// provenance.
    Session {
        title: String,
        detail: String,
        host: String,
        attached: bool,
        attached_here: bool,
    },
    /// A machine; Enter opens a fresh shell on it. `detail` says how it is
    /// reached.
    Host { label: String, presence: TabPresence, detail: String },
    /// A command from the keymap, chord already platform-spelled.
    Action { name: String, chord: String },
    /// Nothing matched the filter; drawn so an empty panel never reads as
    /// broken.
    Nothing,
}

/// Where a text entry's caret sits, as `TextField` describes it.
///
/// Byte offsets into the entry's string — see `SettingsValueCell::Editing`
/// for why bytes and not characters. `Default` is a caret at the start with
/// nothing selected, which is what an untouched box wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caret {
    pub at: usize,
    pub selection: Option<(usize, usize)>,
}

/// The ⌘K palette, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerModel {
    pub rows: Vec<PickerRow>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    /// The live filter string, drawn in the search line.
    pub filter: String,
    /// The filter box's caret and selection.
    pub filter_caret: Caret,
    /// Scroll offset of the row list, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selection into view this pass — keyboard only, so wheel
    /// scrolling never snaps back.
    pub ensure_visible: bool,
    /// How many hosts the query ran over — the query row's right-hand fact.
    pub hosts_searched: usize,
    /// The blink cycle's on half, from the animation clock.
    pub caret_on: bool,
}

/// Which affordance the fleet view's account header offers (issue #190).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetAccountAction {
    /// Nothing clickable — state unknown, or an enrolment in flight.
    None,
    /// "Sign in with a code" — opens the code entry.
    SignIn,
    /// "Sign in with browser" — the hand-off flow (#226): a grant, the
    /// system browser, and a poll until someone clicks Approve.
    SignInBrowser,
    /// "Cancel" — stop waiting on the browser; the grant just expires
    /// server-side.
    CancelLink,
    /// "Sign out" — forgets this app's token.
    SignOut,
}

/// The fleet view's account header, as data: one line of fact, at most one
/// affordance, and the code entry while one is open. Shaped here so
/// `screens.rs` stays declarative-drawing only — the app decides what the
/// account state means, the layout only draws what arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetAccountModel {
    /// The header sentence ("signed in as andy", "not signed in").
    pub line: String,
    pub action: FleetAccountAction,
    /// A second affordance beside the first — the signed-out header offers
    /// both sign-in doors (#226). `None` everywhere one button is the truth.
    pub second: FleetAccountAction,
    /// The enrolment code being typed, drawn with the §11 editing cell
    /// machinery; `None` when no entry is open.
    pub entry: Option<SettingsValueCell>,
    /// The last failure's message, drawn in warn ink beside the retry.
    pub error: Option<String>,
}

/// What a devices-section row offers (issue #190: the app as approver).
///
/// Derived from row state by the app — pending rows approve, approved rows
/// that are not this app's own key vouch — so the drawing stays declarative
/// and one hit region serves both verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetDeviceAction {
    /// Nothing clickable: this app's own key, or an action in flight.
    None,
    /// "Approve" — a pending key becomes trusted on this account's word.
    Approve,
    /// "Vouch" — an already-approved key gets this app's attestation too,
    /// widening which daemons can admit it.
    Vouch,
}

/// One row of the fleet view's devices section, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetDeviceRow {
    pub label: String,
    /// The mono fact line: "browser · pending", "desktop · this app".
    pub detail: String,
    pub action: FleetDeviceAction,
}

/// The fleet view's devices section — hosted account data, present only
/// while the app is signed in.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetDevicesModel {
    pub rows: Vec<FleetDeviceRow>,
    /// The last approval's failure, in the account header's error pattern.
    pub error: Option<String>,
}

/// One host card of the fleet view (design screen 7).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetCard {
    pub name: String,
    /// The window's own machine — accent border, "this machine" note.
    pub local: bool,
    pub online: bool,
    /// A warn pill ("via tunnel"), when the path warrants one.
    pub pill: Option<String>,
    /// Clicking opens a fresh shell on this machine. False when no route
    /// exists yet (an enrolled host the relay dialler cannot reach until
    /// that PR lands) — the card then takes no hit region, because an
    /// affordance that must fail is worse than none.
    pub open: bool,
    /// Label/value rows: path, key, sessions — only what is actually known.
    /// The value colour is by role: 0 plain, 1 success, 2 warn.
    pub rows: Vec<(String, String, u8)>,
    /// The local card's "Enroll this machine" button (issue #227) — present
    /// while the app is signed in, the window's daemon is the real loopback
    /// one, and the account does not yet list this machine.
    pub enroll: Option<FleetEnroll>,
}

/// The enroll button, as drawn: its caption, and whether it answers —
/// `clickable: false` is the in-flight worker saying "not again".
#[derive(Debug, Clone, PartialEq)]
pub struct FleetEnroll {
    pub label: String,
    pub clickable: bool,
}

/// One card of the theme gallery (design screen 8). Colours arrive as raw
/// theme values because the preview renders in *that* theme, not the UI's.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeCard {
    pub id: String,
    pub name: String,
    /// "dark", "light · default".
    pub qualifier: String,
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub accent: [u8; 3],
    pub danger: [u8; 3],
    pub green: [u8; 3],
    /// The normal ANSI row, index order — the swatch strip.
    pub ansi: [[u8; 3]; 8],
    pub active: bool,
}

/// One pane's header facts (design screen 5).
#[derive(Debug, Clone, PartialEq)]
pub struct PaneModel {
    pub host: String,
    /// Mono sub-label: cwd, and the path cost for a remote pane.
    pub sub: String,
    pub focused: bool,
    /// Host-accent slot for the header dot.
    pub accent: usize,
}

/// A full-pane screen replacing the grid (fleet directory, theme gallery).
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenModel {
    Fleet {
        account: FleetAccountModel,
        cards: Vec<FleetCard>,
        /// `None` while signed out — the section simply is not there, since
        /// every row of it is the account's data.
        devices: Option<FleetDevicesModel>,
    },
    Themes { cards: Vec<ThemeCard> },
    /// The Profiles tab's pane: the §12 editor. Boxed because this variant
    /// is an order of magnitude bigger than its siblings and `ScreenModel`
    /// travels by value through the chrome model.
    Profiles(Box<ProfilesScreenModel>),
}

/// The §12 inheritance chip on a profiles-editor row.
///
/// Not `SettingsRowModel`'s provenance tuple, whose `bool` means *warn*: the
/// profiles chips are a different pair of treatments (accent-on-accentSoft
/// for an override, faint-on-header-fill for inheritance), so they travel as
/// their own type in a list parallel to the rows — same-pass built, the
/// picker discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritChip {
    /// Set on the profile itself: "overrides Defaults", accent on accentSoft,
    /// and the only state that earns the 5px modified dot.
    Overrides,
    /// Fell through to `profiles.defaults`: "inherited from Defaults", faint.
    Inherited,
}

/// One scheme option of the §12 swatch picker: a builtin theme's normal ANSI
/// row in index order, read from `zest-theme` — never re-typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeSwatch {
    pub id: String,
    pub ansi: [[u8; 3]; 8],
}

/// One row of the profiles editor's rail (design §12): a launch target.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileRailRow {
    /// Display name — "Defaults" for the reserved parent.
    pub name: String,
    /// The 10px mono sub-line: `command · host`.
    pub sub: String,
    /// Glyph for the 24px tile; `None` draws the placeholder dot.
    pub icon: Option<String>,
    pub accent: AccentChoice,
    /// `1..=9`, right-aligned; `None` on Defaults and past the ninth.
    pub digit: Option<u8>,
}

/// The §12 live preview, as pure data: the mini tab-chip is drawn in the
/// *window's* `ChromeColors` (panel fill, line border — that is the point the
/// caption makes), carrying only the 2px rule and glyph in the profile's
/// accent; the body block is the profile's scheme.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfilePreviewModel {
    /// The chip's title (the profile's name).
    pub title: String,
    pub icon: Option<String>,
    /// The chip's 2px rule and glyph ink.
    pub accent: AccentChoice,
    /// The profile's scheme — the grid's colours, never the chrome's.
    pub scheme_bg: [u8; 3],
    pub scheme_fg: [u8; 3],
    pub scheme_accent: [u8; 3],
    /// The §12 caption, verbatim, naming the window's theme id.
    pub caption: String,
    /// Static content — a uname line in the §12 spirit; no live probe.
    pub lines: Vec<String>,
}

/// The Profiles tab's screen (design §12): a 248px profile rail and an
/// editor column, drawn over the grid area while the Profiles pane is up.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfilesScreenModel {
    /// Rail rows: Defaults pinned first, then one per profile.
    pub rail: Vec<ProfileRailRow>,
    /// Index into `rail` of the profile being edited.
    pub selected_rail: usize,
    /// Editor header: display name, resolved command line, pinned host.
    pub name: String,
    pub command: String,
    pub host_chip: Option<String>,
    pub icon: Option<String>,
    pub accent: AccentChoice,
    /// Defaults has no Delete (§12).
    pub can_delete: bool,
    pub preview: ProfilePreviewModel,
    /// The field rows, sections as `Group` rows — same shape as Settings.
    pub rows: Vec<SettingsRowModel>,
    /// Inheritance chips, index-parallel to `rows` (`None` on non-field rows
    /// and on the launch trio, which never chips).
    pub chips: Vec<Option<InheritChip>>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    pub filter: String,
    /// The filter box's caret and selection.
    pub filter_caret: Caret,
    /// Scroll offset of the rows pane, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selection into view this pass — keyboard only.
    pub ensure_visible: bool,
    /// What to say when a filter matches nothing.
    pub empty: Option<String>,
    /// The footer's override-count sentence, singular handled; the
    /// fall-through sentence on Defaults.
    pub footer_sentence: String,
    /// `[profiles.<name>]`, right-aligned mono in the footer.
    pub table_name: String,
    /// A dropdown menu open on one of `rows` (window.backdrop's).
    pub menu: Option<SettingsMenuModel>,
}

/// Where the + launcher menu hangs from (design §1 / §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherAnchor {
    /// The horizontal strip's `+`: right-anchored under the button. The `+`
    /// sits at the strip's right end since #156, so right-anchoring fits by
    /// construction — the layout still clamps into the window.
    Strip,
    /// The sidebar's `+`: `top: 40, left: 0` from the button, opening
    /// rightwards over the pane (§2's rule) — right-anchored it runs off
    /// the window's left edge.
    Sidebar,
}

/// One row of the + launcher menu (design §1), ready to draw.
///
/// Display-only, the picker discipline: the app keeps a parallel list of
/// actions built in the same pass, so row index `n` here and there mean the
/// same thing by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum LauncherRow {
    /// A launch target. The accent arrives ON the row, resolved to a choice
    /// with [`tab_accent`]'s logic by the app — layout stays pure and a
    /// theme change repaints without rebuilding the menu.
    Profile {
        name: String,
        /// The command line the row runs, resolved through Defaults —
        /// `shell.command` (or the platform default) when the profile
        /// carries none.
        command: String,
        /// The profile's pinned host, drawn as the row's host chip. The
        /// launch honours it (issue #175), so the chip tells the truth:
        /// present exactly when the profile pins a machine, absent when the
        /// row rides the window's route.
        host_label: Option<String>,
        /// The row ⏎ runs, tagged `default` on accentSoft.
        default: bool,
        /// 1–9: the plain-digit chord while the menu is open.
        digit: Option<u8>,
        /// A tab launched from this profile holds the keyboard now.
        active: bool,
        accent: AccentChoice,
    },
    /// The hairline between the launch targets and the two actions.
    Divider,
    /// "Run on another host…" — the fleet picker (⇧⏎).
    RunOnHost,
    /// "Manage profiles" — the Profiles tab. The chord arrives
    /// platform-spelled from the keymap; empty draws no chip (Windows has
    /// no reachable spelling — see `keymap::Mods::SuperShift`).
    ManageProfiles { chord: String },
}

/// One row of a block's ⋯ menu (design §3), ready to draw.
///
/// Display-only, the launcher/picker discipline: the app keeps a parallel
/// action list built in the same pass, so index `n` means the same thing to
/// the renderer and the input path by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockMenuRow {
    /// A verb, with its chord already platform-spelled (empty draws none —
    /// the chrome does not know what a modifier is).
    Action {
        label: String,
        chord: String,
        /// `false` draws the label faint and pushes **no hit region**. An
        /// affordance that answers a click by doing nothing is worse than
        /// none — the rule the fold chevron already had to learn.
        enabled: bool,
    },
    /// The hairline between groups of verbs.
    Divider,
}

/// A block's open ⋯ menu.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockMenuModel {
    pub rows: Vec<BlockMenuRow>,
    /// Index into `rows` the keyboard is on; always an actionable row.
    pub selected: usize,
    /// What the panel hangs off, physical px — the `⋯` rect, or a zero-size
    /// rect at the pointer when a right click opened it.
    pub anchor: [f32; 4],
}

/// The + launcher menu, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct LauncherModel {
    pub rows: Vec<LauncherRow>,
    /// Index into `rows` the keyboard is on; always an actionable row.
    pub selected: usize,
    pub anchor: LauncherAnchor,
}

/// One row of the command palette, ready to draw.
///
/// Display-only, like [`PickerRow`]: the app keeps a parallel list of the
/// actions each row runs, built in the same pass, so index `n` means the
/// same thing to the renderer and the input path by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteRow {
    /// A category header ("Tabs", "Mouse").
    Group { title: String },
    /// A command. `chord` is already platform-spelled by
    /// `keymap::chord_label` (empty when the command has none — the chrome
    /// draws strings, it does not know what a modifier is). Reference rows
    /// — mouse gestures, footnotes — are `runnable: false`, drawn without a
    /// selection affordance and skipped by the keyboard.
    Command { name: String, chord: String, runnable: bool },
}

/// The command palette, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteModel {
    /// Pre-filtered by the app; empty groups never arrive here.
    pub rows: Vec<PaletteRow>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    pub filter: String,
    /// The filter box's caret and selection.
    pub filter_caret: Caret,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selected row into view this pass — keyboard navigation
    /// only, so wheel scrolling never snaps back to the selection.
    pub ensure_visible: bool,
}

/// One face of a font-list cell.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsFace {
    pub family: String,
    /// Past the first resolvable face: dimmed and tagged `fallback` (§11).
    pub fallback: bool,
}

/// The value half of a settings row, as it should be drawn.
///
/// Which cell a field gets is the row builder's decision (from the schema's
/// widget hint); the chrome just draws what arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsValueCell {
    Toggle { on: bool },
    /// ≤3 short, undocumented variants: the segmented control (§11).
    /// `selected` is `None` when the file holds a value no variant matches.
    Segmented { options: Vec<String>, selected: Option<usize> },
    /// A chosen option behind a dropdown — >3 variants, documented ones, or
    /// a roster the client brings (theme picker).
    Select { value: String },
    /// A bounded number: the filled fraction and its numeric text.
    Slider { frac: f32, text: String },
    /// The − / value / ＋ stepper; `text` carries the unit ("14 pt").
    Stepper { text: String },
    /// A string value drawn as a §11 input box — panel fill, hairline
    /// border, click begins the edit. `placeholder` marks a caption standing
    /// in for an unset value ("the host's default shell"): drawn faint, so
    /// what-will-run and what-is-written stay visually distinct.
    Text { text: String, placeholder: bool },
    /// A value the tab displays but does not edit here.
    ReadOnly { text: String },
    /// Stacked font rows; order is the setting, drag is the edit.
    FontList { faces: Vec<SettingsFace> },
    /// Chips with a × each and a dashed add chip.
    TagList { tags: Vec<String> },
    /// Paired key/value cells; an empty value renders `unset` (it unsets).
    KeyValue { entries: Vec<(String, String)> },
    /// A typed edit in progress; drawn as the buffer with a caret, in warn
    /// colours after a failed parse. Whether the edit replaces the value or
    /// grows a list is `settings_ui::EditBuffer`'s business — this cell only
    /// knows how to draw the text.
    ///
    /// `caret` and `selection` are **byte** offsets into `buffer`, straight
    /// off `TextField` — the renderer measures `buffer[..caret]` to place the
    /// caret, so anything but a byte index would have to be converted here,
    /// which is where the off-by-one would live.
    Editing {
        buffer: String,
        caret: usize,
        selection: Option<(usize, usize)>,
        error: bool,
    },
    /// The §12 host pill: status dot, host name, ▾. Clicking opens a typed
    /// edit of the host label today; the fleet-picker chooser the ▾ implies
    /// arrives with the cross-host launch item, which owns the picker's
    /// pending-launch plumbing.
    HostPill { name: String, online: bool },
    /// The §12 scheme picker: one 60×14 eight-swatch chip per builtin theme,
    /// name under it; `selected` is ringed accent on accentSoft.
    SchemeSwatches { options: Vec<SchemeSwatch>, selected: Option<usize> },
    /// The §12 tab-colour row: six 22px swatches from the window theme's
    /// accent roster (resolved at draw time — the model stays theme-free).
    /// `inert` dims them to 35% and drops their hit regions: the host
    /// decides, and a control that acts while claiming not to would lie.
    AccentSwatches { selected: Option<u8>, inert: bool },
    /// The §12 icon row: 26px glyph tiles, one per roster entry.
    Glyphs { options: Vec<String>, selected: Option<usize> },
}

/// One row of the settings tab, ready to draw.
///
/// Display-only, like [`PickerRow`]: the app keeps a parallel action list
/// built in the same pass, so index `n` means the same thing to the renderer
/// and the input path by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsRowModel {
    /// A group header ("Text", "Window", …).
    Group { title: String },
    /// One setting.
    Setting {
        /// Humanized field name ("Font size").
        label: String,
        /// The dotted key, drawn faint — it is what the user greps their
        /// config for.
        key: String,
        /// First line of the field's doc comment.
        description: String,
        value: SettingsValueCell,
        /// `("set by profile `k8s`", warn)` — warn when the source outranks
        /// the user's file, because an edit there would be shadowed.
        provenance: Option<(String, bool)>,
        /// Changing this applies on the next launch.
        restart: bool,
        /// Declared in the schema but not consumed by the app yet.
        inert: bool,
        /// Differs from the schema default.
        modified: bool,
    },
    /// A banner pinned above the list — restart owed, or a failed write.
    Notice { text: String },
    /// One key the cascade kept but the schema does not know (§11's ninth
    /// category): the key in mono, which layer set it, and a suggestion when
    /// one is close enough to be a plausible typo.
    Unknown { key: String, source: String, suggestion: Option<String> },
}

/// One category of the settings tab's rail.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsCategoryModel {
    pub label: String,
    /// Fields in this group that differ from their defaults — the rail's
    /// right-aligned count, blank at zero.
    pub modified: usize,
}

/// One option of an open dropdown menu.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsMenuOption {
    /// Humanized label ("Mica").
    pub label: String,
    /// The kebab wire value, drawn in mono ("mica").
    pub value: String,
    /// The variant's doc comment; empty draws nothing.
    pub doc: String,
}

/// A dropdown menu open on one settings row.
///
/// One menu for both kinds of choice: the schema's own variants, and the
/// rosters the client brings (themes, installed families). Before #259 the
/// second kind had no dropdown at all — a ▾ pill opened the ⌘K command
/// palette, placeholder "type to run a command" included — because the menu
/// could only read `field.variants`, which a roster is not.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsMenuModel {
    /// Row index the menu is anchored to.
    pub row: usize,
    /// Already filtered by the app — the same-pass discipline `PaletteRow`
    /// and `PickerRow` use, so index `n` means the same thing to the
    /// renderer and to the input path by construction.
    pub options: Vec<SettingsMenuOption>,
    /// Index of the current value (the ✓), when it matches a *visible*
    /// option.
    pub current: Option<usize>,
    /// Index into `options` the keyboard is on.
    pub selected: usize,
    /// Draw the search row. Off for a handful of documented variants, where
    /// a search box over four rows is noise; on for a roster.
    pub searchable: bool,
    pub filter: String,
    pub filter_caret: Caret,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selection into view this pass — keyboard only, so the wheel
    /// scrolls freely.
    pub ensure_visible: bool,
    /// A last row that is not an option: "Browse all themes…", which opens
    /// the gallery (design screen 8). Empty draws nothing.
    pub footer: Option<String>,
}

/// The Settings tab's screen (design §11): category rail + content column,
/// drawn over the grid area while the active tab is Settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsScreenModel {
    /// Rail rows, in GROUP_ORDER + "Unknown keys"; empty categories are
    /// already hidden by the app when a filter is live.
    pub categories: Vec<SettingsCategoryModel>,
    /// Index into `categories`.
    pub selected_category: usize,
    /// Content header: the group's name, its dotted prefix, and a lede.
    pub heading: String,
    pub prefix: String,
    pub lede: String,
    /// Rows of the selected category (no group headers — the rail is the
    /// grouping now), banners first.
    pub rows: Vec<SettingsRowModel>,
    /// What to say when `rows` is empty (a clean unknown-keys category, or a
    /// filter that matches nothing) — a blank panel reads as broken.
    pub empty: Option<String>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    pub filter: String,
    /// The filter box's caret and selection.
    pub filter_caret: Caret,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selected row into view this pass. Keyboard only — the
    /// wheel must scroll freely without snapping back.
    pub ensure_visible: bool,
    /// Footer: how many settings differ from the defaults, fleet-wide over
    /// every category, and where the file lives.
    pub modified_total: usize,
    pub config_path: String,
    /// A dropdown menu open on one of `rows`, drawn over everything.
    pub menu: Option<SettingsMenuModel>,
}

/// How a tab's host is currently reached, carried per tab since the status
/// bar's deletion (design §1: "no status bar").
///
/// `Stalled` and `Reconnecting` describe the *link*, not the path — a LAN
/// session mid-hiccup is stalled, whatever route it normally takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Loopback,
    Lan,
    Tunnel,
    Stalled,
    Reconnecting,
}

/// The animation clock's current phases, computed by the app per rebuild.
/// The four design animations, minus hover (instant by spec): caret blink,
/// spinner rotation, running-dot pulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimPhase {
    /// The blink cycle's on half — the palette caret and the grid cursor.
    pub caret_on: bool,
    /// Spinner rotation, 0..1 of a 0.9s turn.
    pub spin: f32,
    /// Running-dot opacity, 0.35..1 on a 1.6s ease.
    pub pulse: f32,
}

impl Default for AnimPhase {
    fn default() -> Self {
        Self { caret_on: true, spin: 0.0, pulse: 1.0 }
    }
}

/// Everything `layout` needs to draw the chrome once.
/// What the window's own controls take out of the chrome, whoever draws them.
///
/// The asymmetry between the two platforms is the point, not an accident.
/// macOS injects a *measurement*, because AppKit is the only authority on
/// where the traffic lights are — they move with OS version and localization.
/// Windows injects only a *flag*, because we draw the buttons ourselves and
/// their width is a layout constant the layout pass already knows. One
/// `Option` and one `bool`, rather than a third concept for each edge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowControls {
    /// Physical px the *OS* draws over our chrome at the leading edge, and the
    /// bar height it wants: `[width, height]`. The macOS traffic lights.
    /// `None` in fullscreen, where they auto-hide, and on every other platform.
    pub native_leading: Option<[f32; 2]>,
    /// We draw minimise / maximise / close ourselves at the trailing edge.
    pub drawn_caption: bool,
    /// The maximise affordance shows the restore glyph instead.
    pub maximized: bool,
    /// The window can be resized by dragging its edges.
    ///
    /// False when maximized, in fullscreen, and on the native-frame path where
    /// the OS owns them — a borderless window has no non-client area left for
    /// `DefWindowProc` to answer `HTLEFT` from, so the edges have to come out
    /// of our own hit map or they do not exist at all.
    pub resizable_edges: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeModel {
    pub tabs: Vec<TabModel>,
    /// Index into `tabs`.
    pub active: usize,
    pub position: TabsPosition,
    /// Scroll offset of the tab strip contents, physical pixels. Layout
    /// clamps it and reports the clamped value back.
    pub strip_scroll: f32,
    /// Bring the active chip into the strip's viewport this pass — set on
    /// activation paths only, so wheel scrolling never snaps back. The same
    /// discipline as the picker's `ensure_visible`.
    pub ensure_active_visible: bool,
    /// What the pointer is over, from last frame's hit map. Only used for
    /// hover fills, so one frame of lag is invisible.
    pub hover: Option<HitRegion>,
    /// What the window's own controls take out of the chrome.
    pub controls: WindowControls,
    pub focused: bool,
    /// Sidebar grouping and footer counts; `None` in the horizontal layout.
    pub sidebar: Option<SidebarModel>,
    /// A full-pane screen over the grid area (fleet, themes); Esc returns.
    pub screen: Option<ScreenModel>,
    /// The split tab's two pane headers, left then right; `None` unsplit.
    pub panes: Option<[PaneModel; 2]>,
    /// Animation phases for this rebuild.
    pub anim: AnimPhase,
    /// Where the grid area is, physical pixels — the rectangle a screen
    /// covers. Computed by the app from its insets.
    pub grid_area: [f32; 4],
    /// The palette pill's chord, platform-spelled ("⌘K") — composed by the
    /// app because the chrome does not know what a modifier is.
    pub palette_chord: String,
    /// The settings chord ("⌘,"), for the vertical sidebar's pinned row.
    pub settings_chord: String,
    /// The fleet picker, drawn over everything when open.
    pub picker: Option<PickerModel>,
    /// The command palette, likewise modal. The app enforces that at most
    /// one overlay is open, so layout never has to rank them.
    pub palette: Option<PaletteModel>,
    /// The Settings tab's screen — not an overlay: drawn over the grid area
    /// (like `screen`) while the active tab is Settings, with the modals
    /// still able to open above it.
    pub settings: Option<SettingsScreenModel>,
    /// The + launcher menu — it joins the at-most-one-overlay set the app
    /// enforces, so layout never has to rank it against the others. While
    /// it is open the `+` wears selSoft fill and accent ink (design §1).
    pub launcher: Option<LauncherModel>,
    /// A window-level line the user must be able to read *now* — today, the
    /// pairing approval prompt ("waiting for approval on forge — code
    /// 481502"), which exists while an attach worker blocks on a person at
    /// the other machine (#190). Drawn pinned to the top of the grid area,
    /// under the modal overlays; `None` draws nothing.
    pub notice: Option<String>,
    /// The pairing approval modal (ROADMAP M4): a device is waiting for a
    /// person at THIS machine to let it in. Drawn over everything, including
    /// the other modals — its text is a security decision, and chrome that
    /// could cover it would be chrome that could spoof it by omission.
    pub approval: Option<ApprovalModel>,
}

/// What the approval modal says. Composed by the app (which holds the
/// request and its clock); layout only draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalModel {
    /// The asking device's self-declared name ("andy-phone") — signed into
    /// the transcript, so it is at least the device's own claim.
    pub label: String,
    /// Where it is connecting from ("192.168.1.42:60123").
    pub remote: String,
    /// The six-digit matching code; the person compares it with the one on
    /// the asking device's screen.
    pub code: String,
    /// Pre-formatted validity line ("code expires in 2m").
    pub expires: String,
}

/// The knobs `layout` reads, resolved to physical pixels by the caller.
///
/// Text measurement comes in as data too: the pure layout cannot shape, so
/// the app measures the strings it is about to lay out (via
/// `zest_render_wgpu::measure_ui_run`) and the tests measure with arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeMetrics {
    /// Window size, physical pixels.
    pub width: f32,
    pub height: f32,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// `tabs.strip_height`, logical.
    pub strip_height: f32,
    /// `tabs.sidebar_width`, logical.
    pub sidebar_width: f32,
    /// Height of one line of UI text, physical (the grid's cell height).
    pub line_height: f32,
    /// Baseline offset from the top of a text line, physical.
    pub baseline: f32,
    /// The grid font's size, physical pixels — the default for any text run
    /// the design does not size explicitly.
    pub font_px: f32,
}

impl ChromeMetrics {
    /// The strip's extent in physical pixels along its defining axis.
    #[must_use]
    pub fn strip_extent(&self, position: TabsPosition) -> f32 {
        match position {
            TabsPosition::Top => self.strip_height * self.scale,
            TabsPosition::Left => self.sidebar_width * self.scale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(color_from: Option<ColorFrom>, tab_color: Option<u8>) -> ProfileIdentity {
        ProfileIdentity {
            name: "test".into(),
            scheme: None,
            selection_bg: None,
            tab_color,
            icon: None,
            color_from,
            opacity: None,
            title: zest_config::TabTitle::FromShell,
        }
    }

    #[test]
    fn tab_accent_prefers_the_profiles_own_colour_unless_the_host_decides() {
        // The §12 truth table. Getting the unset default wrong makes a
        // profile that only set tab_color look like it never did.
        use AccentChoice::{Host, Profile};
        let cases = [
            (Some(ColorFrom::Profile), Some(3), Profile(3)),
            // A profile told to use its own colour without picking one has
            // nothing to draw but its host's.
            (Some(ColorFrom::Profile), None, Host(2)),
            // Host wins even over a picked colour — that is what the
            // fleet-reads-by-machine setting means.
            (Some(ColorFrom::Host), Some(3), Host(2)),
            (Some(ColorFrom::Host), None, Host(2)),
            // Unset color_from defaults to the profile's own colour (§12).
            (None, Some(3), Profile(3)),
            (None, None, Host(2)),
        ];
        for (from, color, want) in cases {
            assert_eq!(
                tab_accent(Some(&identity(from, color)), 2),
                want,
                "color_from={from:?} tab_color={color:?}"
            );
        }
        assert_eq!(
            tab_accent(None, 2),
            Host(2),
            "a tab launched from no profile shows its host"
        );
    }
}
