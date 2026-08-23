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

/// A session nobody has named.
///
/// One constant because this was six string literals across the app and one
/// more in the web client, and a fallback spelled differently in two places
/// is two fallbacks.
pub const UNNAMED_SESSION: &str = "shell";

/// The longest label anything keeps.
///
/// Far beyond what any chip can show (~30 characters at the 168px basis), so
/// it never truncates anything a person would see. It exists because chrome
/// text is re-shaped on *every presented frame* with no shaping cache
/// (`ui_text::emit_ui_run`): without a bound, one pasted multi-kilobyte
/// command would pay for itself sixty times a second for as long as its tab
/// is open.
const MAX_LABEL_BYTES: usize = 512;

/// What a session is called, in one place.
///
/// Precedence: **the last command the shell told us about, then the OSC 0/2
/// title, then [`UNNAMED_SESSION`]**. The command wins because a tab you left
/// an hour ago has to answer "what did I run here", which a title of `zsh` —
/// or, under the default macOS zsh, the same cwd as every other tab — never
/// did. And it *keeps* the finished command until the next one starts: a chip
/// that reverts the moment a build ends is a chip that forgets the thing you
/// came back to read.
///
/// This is what [`zest_config::profiles::TabTitle::FromShell`] should always
/// have meant; the other two variants are still unapplied (see ROADMAP).
#[must_use]
pub fn session_label(command: &str, osc_title: &str) -> String {
    let name = session_name(command, osc_title);
    if name.is_empty() {
        UNNAMED_SESSION.to_string()
    } else {
        name
    }
}

/// The precedence without the fallback: empty when the session has said
/// nothing about itself.
///
/// The OS titlebar wants this one — its default is the application's name,
/// not `shell` — and it is the only caller that has a better answer than
/// [`UNNAMED_SESSION`] to fall back to.
fn session_name(command: &str, osc_title: &str) -> String {
    let command = one_line(command);
    if !command.is_empty() {
        return command;
    }
    one_line(osc_title)
}

/// [`session_label`] over a live terminal: both facts under the one lock the
/// caller already holds.
#[must_use]
pub fn terminal_label(term: &zest_core::Terminal) -> String {
    session_label(command_of(term), term.title())
}

/// [`session_name`] over a live terminal — empty when unnamed.
#[must_use]
pub fn terminal_name(term: &zest_core::Terminal) -> String {
    session_name(command_of(term), term.title())
}

fn command_of(term: &zest_core::Terminal) -> &str {
    term.blocks().last_command().map_or("", |b| b.command.as_str())
}

/// Flatten text that will be drawn as a single line.
///
/// Both inputs need it and for the same reason: `OSC 633;E` un-escapes `\x0a`,
/// so a multiline command is an ordinary VS Code-integration case, and an
/// `OSC 2` title is whatever a program chose to send. Control bytes become
/// spaces — the `tabs::sanitize` rule, which exists because text drawn from
/// elsewhere must not carry an escape back into the pane reporting it — and
/// runs of whitespace collapse so `for f in *; do` does not arrive with the
/// newline's worth of indentation after it.
fn one_line(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if c.is_control() || c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if out.len() + usize::from(space) + c.len_utf8() > MAX_LABEL_BYTES {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
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
    /// `host` is the machine's identity; `label` is only what to call it.
    /// Carried together (#304) because the variant used to offer the label
    /// alone, and every lookup downstream reached for the one field it had —
    /// two machines may share a display name. The id is all-zero while a
    /// launch is still connecting (a placeholder address has no id yet), and
    /// the label is then the only key there is.
    Remote { host: zest_proto::HostId, label: String },
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
    /// Already derived by [`session_label`]: the last command, else the OSC
    /// title, else `shell`.
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
    /// A command is currently running in this session — the pulsing dot.
    ///
    /// From OSC 133 blocks, so it is silent under a shell with no integration
    /// and under the alternate screen. [`Self::progress`] is the other half:
    /// what the program says about itself rather than what the shell says
    /// about it, and neither implies the other.
    pub running: bool,
    /// What a long job in this session last said about itself (`OSC 9;4`).
    pub progress: zest_core::Progress,
    /// This session asked to be noticed while you were looking elsewhere.
    ///
    /// The cause rides along rather than collapsing to a `bool` because the
    /// chrome should be able to say *why* it is showing a dot, and "which of
    /// these was it" is not recoverable from the fact that something happened
    /// (the discipline `ExitSource` follows in ADR-015).
    pub attention: Option<zest_proto::AttentionCause>,
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
    /// This tab's profile override of `window.opacity`, when it has one
    /// (`ProfileIdentity::opacity`, the same value `pane_opacity` gives the
    /// pane). `None` follows the window.
    ///
    /// The active chip *is* the pane drawn a few pixels higher, so it has to
    /// be as solid as that pane and not as the window around it — a profile
    /// pinned to 0.5 in an opaque window would otherwise read as a solid tab
    /// over a see-through pane.
    pub opacity: Option<f32>,
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

/// How far the palette's fleet-wide block search has got (#527).
///
/// `answered` is the number the row prints: a host that has not answered
/// has not been searched, whatever the fleet listing knows about it. Until
/// this existed the row counted every known fleet row while the blocks came
/// from attached tabs — the number lying beside the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostsSearched {
    pub answered: usize,
    /// Hosts the question reached. While `answered < asked` the row says
    /// `2 of 3 hosts searched`, so a slow relay reads as pending rather
    /// than as a fleet that is smaller than it is.
    pub asked: usize,
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
    /// The query row's right-hand fact: how many hosts have answered, over
    /// how many were asked.
    pub hosts_searched: HostsSearched,
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
    /// What is running on this machine, one row each (#287).
    ///
    /// The ⌘K picker could attach to a remote session before the screen that
    /// exists to *show you the fleet* could. Empty for a host that has told us
    /// nothing — which is not the same as a host with no sessions, and the
    /// `sessions` count row above is what distinguishes them.
    pub sessions: Vec<FleetSessionRow>,
    /// How many sessions the list left out.
    ///
    /// A card is a summary, not an inventory: a machine running thirty shells
    /// would otherwise make every card in the grid thirty rows tall, since the
    /// grid is uniform-height. The drawn "+N more" row says so out loud and
    /// points at ⌘K, which does hold them all — a cap nobody is told about is
    /// a card that quietly lies about what is running.
    pub sessions_hidden: usize,
}

/// One session on a fleet card, ready to click.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetSessionRow {
    pub title: String,
    /// The working directory, home-shortened for this machine only — another
    /// machine's home is unknowable from here.
    pub detail: String,
    /// A client is attached to it somewhere, not necessarily this one.
    pub attached: bool,
    /// This window already holds it: clicking activates that tab rather than
    /// opening a second view of one session.
    pub here: bool,
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
    /// The pane's name in its header: a host for a shell, a filename for a
    /// file (#464).
    pub host: String,
    /// Mono sub-label: cwd and the path cost for a remote pane; the
    /// containing directory for a file.
    pub sub: String,
    pub focused: bool,
    /// Host-accent slot for the header dot.
    pub accent: usize,
    /// What the pane holds, for the parts of the header that differ.
    pub kind: PaneKind,
}

/// What a pane is showing — the header's only branch, and the reason the
/// layout pass does not have to ask the tab anything.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneKind {
    Session,
    Editor(EditorView),
}

/// Everything the layout pass needs to draw an open file, and nothing it does
/// not (#464).
///
/// The lines are the **visible** ones, already sliced and tab-expanded by the
/// app: the layout pass is pure and measure-injected, and handing it a hundred
/// thousand lines to skip past every frame would make it the wrong kind of
/// pure. Slicing where the geometry is known also keeps the gutter's numbers
/// and the rows they sit beside in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    /// Line number of `lines[0]`, 1-based as a gutter counts.
    pub first_line: usize,
    pub lines: Vec<String>,
    /// Lines in the whole file, which is what the gutter is sized for — a file
    /// scrolled to line 9 must not have the gutter narrow under it.
    pub total: usize,
    pub scroll_x: f32,
    /// Shown as a badge, before someone tries to type rather than after.
    pub readonly: bool,
    /// More file exists than arrived.
    pub truncated: bool,
    /// There is nothing to draw but a reason: still opening, refused by the
    /// host, or not text. Drawn instead of the lines, never beside them.
    pub notice: Option<String>,
}

/// The "Open file…" prompt (#464): one path entry and, after a refusal, the
/// reason it did not open.
///
/// No completion list yet — #449's `ListDir` lists directories only, and half
/// a completion (folders but not files) is worse than none, because the
/// missing half looks like the file is not there.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenFileModel {
    pub path: String,
    pub caret: Caret,
    /// The directory a relative path will resolve against, shown so the
    /// person can tell which machine and which directory they are typing
    /// into — the two things a path alone does not say.
    pub cwd: String,
    /// The machine that will do the reading.
    pub host: String,
}

/// The find bar, while it is open (#519).
///
/// A floating panel in the focused pane's top-right — not docked above the
/// grid, which would resize the pty (ADR-013's most expensive operation) and
/// reflow the very text being searched, and not at the bottom, where the prompt
/// chips and the live prompt are and `prompt_chips`'s never-overlay rule
/// protects the caret.
#[derive(Debug, Clone, PartialEq)]
pub struct FindBarModel {
    pub query: String,
    pub caret: Caret,
    /// `3 of 47`, `3 of 5000+`, or `No results` — never `0 of 0`.
    pub count: String,
    /// No hits, so the count is drawn in `danger` rather than `faint`.
    pub empty: bool,
    /// The `Aa` chip is lit.
    pub case_sensitive: bool,
    /// History is still arriving from the host, so the count is still
    /// growing (#545).
    pub fetching_history: bool,
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
    Themes {
        cards: Vec<ThemeCard>,
        /// Why the last clipboard import was refused — drawn inside the
        /// dashed card, in place of its hint line. `None` after a success:
        /// the new card appearing *is* the success feedback.
        import_error: Option<String>,
    },
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
    /// Not about inheritance at all: this row reaches a session when it
    /// *starts*, so changing it leaves every open tab as it was.
    ///
    /// Deliberately not the Settings screen's `restart` chip, which says
    /// "needs a restart" in warning colours: nothing here needs zesterm
    /// restarted, and a badge that over-claims is one people learn to ignore.
    /// A process cannot be handed a new environment on any operating system —
    /// WezTerm's docs say exactly this, in prose, next to the option; saying
    /// it in the editor instead is the same fact where someone is actually
    /// looking.
    NewSessions,
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

/// A profile rename in flight (§12, #283).
///
/// `caret` and `selection` are **byte** offsets into `buffer`, straight off
/// `TextField` — the same contract `SettingsValueCell::Editing` states, and
/// for the same reason: the renderer measures `buffer[..caret]` to place the
/// caret, so anything but a byte index would have to be converted here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileNameEdit {
    pub buffer: String,
    pub caret: usize,
    pub selection: Option<(usize, usize)>,
    /// Why the name cannot be committed, shown under the entry. The box wears
    /// the warn border while this is `Some`, exactly like a settings row that
    /// failed to parse.
    pub error: Option<String>,
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
    /// A rename in progress (§12, #283): the buffer replaces the header name
    /// with a text entry. `None` is the resting state, and Defaults never
    /// carries one — the reserved parent cannot be renamed.
    pub renaming: Option<ProfileNameEdit>,
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
    /// A host header: the machine the rows under it will run on (#268).
    ///
    /// Present only when the menu spans more than one machine — a
    /// single-machine setup, which is most setups, must not grow chrome to say
    /// so. Never actionable: the keyboard skips it and it takes no hit region,
    /// because "which machine" is context here, not a thing to click.
    Group {
        /// `THIS MACHINE · studio`, `FORGE`, `ANY MACHINE`.
        label: String,
        /// The mono sub-label: `windows · LAN 0.4 ms`, `asleep`. Empty draws
        /// none.
        sub: String,
        /// Drives the status dot, exactly as the sidebar's host headers do.
        online: bool,
    },
    /// A faint explanatory line under a host header (#537): the machine is
    /// enrolled and online but its watcher has not delivered yet, so its
    /// published profiles cannot be listed — and silence here reads as "the
    /// feature does not exist". Never actionable: the keyboard skips it and
    /// it takes no hit region, like the header above it.
    Note { text: String },
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

/// The cwd chip's directory browser (#439), ready to draw.
///
/// Rows are plain labels: the `..` row first when the path has a parent —
/// drawn faint, because it *navigates* where every other row switches —
/// then the filtered children. The app keeps the parallel answer list
/// (which path each row means), built in the same pass, so index `n` means
/// one thing to the renderer and the input path by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct DirPickerModel {
    pub rows: Vec<String>,
    /// Row 0 is the parent row.
    pub has_parent: bool,
    pub selected: usize,
    pub filter: String,
    pub filter_caret: Caret,
    pub scroll: f32,
    pub ensure_visible: bool,
    /// The host's answer has not arrived yet.
    pub loading: bool,
    /// Why the listing is empty when it is empty for a reason.
    pub error: String,
    /// More existed than the rows carry.
    pub truncated: bool,
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
    /// A path to a local file: the same input box, plus a `Browse…` button.
    ///
    /// Its own variant rather than a flag on `Text` because the two differ in
    /// *width* as well as in what they carry — the button takes room the input
    /// would otherwise have — and `control_height` and `draw_control` are both
    /// exhaustive, so a variant is what makes the next reader of either notice
    /// there is a second shape.
    /// A path to a local file: the same input box, plus a `Browse…` button.
    ///
    /// Its own variant rather than a flag on `Text` because the two differ in
    /// *width* as well as in what they carry — the button takes room the input
    /// would otherwise have — and `control_height` and `draw_control` are both
    /// exhaustive, so a variant is what makes the next reader of either notice
    /// there is a second shape.
    FilePath { text: String, placeholder: bool },
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
    /// The split tab's pane headers, left to right (the primary first);
    /// `None` unsplit.
    pub panes: Option<Vec<PaneModel>>,
    /// Animation phases for this rebuild.
    pub anim: AnimPhase,
    /// Where the grid area is, physical pixels — the rectangle a screen
    /// covers. Computed by the app from its insets.
    pub grid_area: [f32; 4],
    /// The palette pill's chord, platform-spelled ("⌘K") — composed by the
    /// app because the chrome does not know what a modifier is.
    pub palette_chord: String,
    /// The Settings chord ("⌘,"), for its sidebar row's right slot.
    pub settings_chord: String,
    /// The Profiles chord ("⌘⇧,"), for the same slot on its own row. A
    /// second field rather than a lookup: the chrome does not know what a
    /// modifier is, which is why the app spells both.
    pub profiles_chord: String,
    /// The fleet picker, drawn over everything when open.
    /// The "Open file…" prompt, while it is up (#464).
    pub open_file: Option<OpenFileModel>,
    /// The find bar, when open (#519).
    pub find: Option<FindBarModel>,
    pub picker: Option<PickerModel>,
    /// The command palette, likewise modal. The app enforces that at most
    /// one overlay is open, so layout never has to rank them.
    pub palette: Option<PaletteModel>,
    /// The cwd chip's directory browser, when open (#439).
    pub dir_picker: Option<DirPickerModel>,
    /// The Settings tab's screen — not an overlay: drawn over the grid area
    /// (like `screen`) while the active tab is Settings, with the modals
    /// still able to open above it.
    pub settings: Option<SettingsScreenModel>,
    /// The + launcher menu — it joins the at-most-one-overlay set the app
    /// enforces, so layout never has to rank it against the others. While
    /// it is open the `+` wears selSoft fill and accent ink (design §1).
    pub launcher: Option<LauncherModel>,
    /// A block's ⋯ menu, when open (design §3).
    pub block_menu: Option<BlockMenuModel>,
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
    /// A ⌘W that landed on a busy tab and is waiting for an answer (#381).
    /// Below the approval modal — that one is a security decision and this
    /// one is not — and above everything else, because the keystroke that
    /// opened it was the user's own.
    pub confirm_close: Option<ConfirmCloseModel>,
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

/// The close-a-busy-tab question (#381).
///
/// The words are composed by the app and drawn verbatim — the same division
/// [`ApprovalModel::expires`] and [`TabModel::age`] already follow, because
/// only the app knows what the tab is running and whether there is a daemon to
/// leave it with, and a layout that assembled sentences would need to know
/// both.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmCloseModel {
    /// Which tab this is about. Carried so the answer cannot be applied to a
    /// different tab than the one the question named — the strip can change
    /// underneath an open modal.
    pub addr: SessionAddr,
    /// The heading — `Close “vim”?`.
    pub title: String,
    /// The sentence under it, or empty for none.
    pub body: String,
    /// The faint line under that: what the other answer would do, or why
    /// there is no other answer.
    pub hint: String,
    /// Which answers this question actually has.
    pub choices: ConfirmChoices,
}

/// The buttons a [`ConfirmCloseModel`] offers.
///
/// An enum rather than a pair of flags because only three of the four
/// combinations mean anything — and the fourth, a question with nothing to
/// answer it, is exactly the one a pair of flags makes reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoices {
    /// Detach, Close and stop it, Cancel. A busy tab whose session a daemon
    /// is holding: all three outcomes exist.
    DetachOrClose,
    /// Close and stop it, Cancel. A busy tab this window owns outright, so
    /// there is nothing to leave the shell with. A Detach button here would
    /// be a button for an outcome the build cannot produce, and the person
    /// would believe the shell survived.
    CloseOnly,
    /// Cancel alone — really "OK". ⌘B on a tab with no daemon: the answer is
    /// *no*, and offering to end the shell instead would be answering a
    /// question nobody asked, one destructive click away from a gesture that
    /// promised not to.
    Acknowledge,
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
    /// Width of one grid cell, physical (#464).
    ///
    /// `line_height` has always been the cell's *height*; a file pane needs
    /// the other axis too, so its gutter is a whole number of columns wide and
    /// a sideways flick moves it the same distance it moves a terminal.
    pub cell_w: f32,
    /// `window.padding`, logical — what `pane_body` insets by, so the layout
    /// pass can find a pane's body without being handed the config.
    pub padding: u32,
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
            background_image: None,
            background_fit: None,
            background_dim: None,
            title: zest_config::TabTitle::FromShell,
        }
    }

    #[test]
    fn a_session_is_named_by_what_it_last_ran() {
        // The precedence table. The command wins because a tab you left an
        // hour ago has to answer "what did I run here" -- under the default
        // macOS zsh the OSC title is the cwd, so without this every tab in a
        // window reads the same.
        let cases = [
            ("cargo build", "~/dev/zesterm", "cargo build"),
            ("", "~/dev/zesterm", "~/dev/zesterm"),
            // A blank command is not a name; the title still is.
            ("   ", "~/dev/zesterm", "~/dev/zesterm"),
            ("cargo build", "", "cargo build"),
            ("", "", UNNAMED_SESSION),
            // ...and neither is a blank title.
            ("", "  \t ", UNNAMED_SESSION),
        ];
        for (command, title, want) in cases {
            assert_eq!(
                session_label(command, title),
                want,
                "session_label({command:?}, {title:?})"
            );
        }
    }

    #[test]
    fn an_unnamed_session_leaves_the_os_titlebar_to_its_own_default() {
        // The one caller with a better answer than "shell": a window with
        // nothing to say is called zesterm, which is why the precedence and
        // the fallback are two functions.
        assert!(session_name("", "").is_empty());
        assert_eq!(session_name("", "~/dev"), "~/dev");
    }

    #[test]
    fn a_multiline_command_stays_one_line() {
        // Not theoretical: OSC 633;E un-escapes \x0a, so a multiline command
        // is an ordinary VS Code-integration case. A raw newline in a chip
        // draws a second line over the strip; a raw escape drawn back into a
        // pane would repaint it.
        let label = session_label("for f in *; do\n  echo $f\r\ndone", "");
        assert_eq!(label, "for f in *; do echo $f done");
        assert!(
            !label.chars().any(char::is_control),
            "no control byte survives into a label: {label:?}"
        );
        // C0, DEL and C1 alike — `char::is_control` covers all three, and C1
        // is where the web mirror of this rule diverged before review caught
        // it (`chrome-model.ts`, PR #535). The pair is checked in both
        // languages so the next divergence fails somewhere.
        let c1 = session_label("ls\u{85}-la\u{9b}31m", "");
        assert_eq!(c1, "ls -la 31m", "a C1 byte is a control byte");
    }

    #[test]
    fn a_pasted_script_is_cut_on_a_char_boundary() {
        // Chrome text is re-shaped every presented frame with no cache, so
        // the bound is a per-frame cost guard, not a display decision -- it
        // sits far beyond anything a chip can show.
        let long = "\u{e9}".repeat(4096);
        let label = session_label(&long, "");
        assert!(label.len() <= MAX_LABEL_BYTES, "bounded: {}", label.len());
        assert!(
            label.chars().all(|c| c == '\u{e9}'),
            "cut on a character boundary, never mid-codepoint"
        );
    }

    #[test]
    fn terminal_label_reads_the_block_then_the_title() {
        // The whole lifecycle over a real parser, because the rule is about
        // *which* block answers and that is decided by OSC 133 alone.
        let mut term = zest_core::Terminal::new(80, 24, 100);
        term.advance(b"\x1b]2;~/dev/zesterm\x07");
        assert_eq!(terminal_label(&term), "~/dev/zesterm", "no blocks yet");

        term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build\x1b]133;C\x07\r\n");
        assert_eq!(terminal_label(&term), "cargo build", "while it runs");

        term.advance(b"   Compiling zest-app\r\n\x1b]133;D;0\x07");
        assert_eq!(
            terminal_label(&term),
            "cargo build",
            "and after it finishes -- a chip that reverts forgets the thing \
             you came back to read"
        );

        term.advance(b"\x1b]133;A\x07$ ");
        assert_eq!(
            terminal_label(&term),
            "cargo build",
            "the prompt after it names nothing, so the command still stands"
        );

        term.advance(b"\x1b]133;B\x07cargo test\x1b]133;C\x07\r\n");
        assert_eq!(terminal_label(&term), "cargo test", "until the next one starts");
    }

    #[test]
    fn a_connecting_tab_still_reads_its_profile() {
        // A pane that has not connected has no blocks, and the profile name
        // was written into it as an OSC 2 (`tabs::PendingSession`). The
        // command precedence must not turn that into "shell".
        let mut term = zest_core::Terminal::new(80, 24, 0);
        term.advance(b"\x1b]2;Ubuntu\x07");
        assert_eq!(terminal_label(&term), "Ubuntu");
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
