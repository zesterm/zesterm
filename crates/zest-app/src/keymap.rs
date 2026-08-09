//! Every chord the app owns, in one table.
//!
//! Before this table, shortcuts were unnamed if-blocks in `window_event` — a
//! shape that cannot be enumerated, so nothing could *display* the shortcuts
//! without hand-maintaining a second list that drifts the first time anyone
//! adds a chord. The table is what the dispatch consults and what the
//! command palette renders *and runs*, so an unlisted chord structurally
//! cannot exist and a palette row cannot do anything but what its chord does.
//! It is also the rail user-configurable keybindings would layer onto: a
//! config section becomes data merged over [`BINDINGS`], not a rewrite.
//!
//! Modal overlays (the picker's type-to-filter, for instance) stay outside the
//! table on purpose: their keys are a line editor, not commands, and "any
//! character appends to the filter" is not expressible as a chord row.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use zest_input::key;

use crate::chrome::model::PaletteRow;

/// What the user asked the *app* to do — never the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewTab,
    CloseTab,
    ToggleFleetPicker,
    /// 0-based; ⌘1..⌘8.
    ActivateTab(u8),
    /// ⌘9 is "last", following the browsers everyone's fingers learned it
    /// from.
    ActivateLastTab,
    PrevTab,
    NextTab,
    Copy,
    Paste,
    CopyBlockOutput,
    RerunLastCommand,
    ScrollPageUp,
    ScrollPageDown,
    /// The command palette itself — rendered from this table, so it can
    /// never list a chord that does not exist, and every row it lists runs
    /// through the same dispatch the chord does.
    TogglePalette,
    /// The settings overlay (⌘, — the desktop's own convention).
    ToggleSettings,
}

/// The modifier half of a chord, as *policy* rather than bitmask.
///
/// Two of these deliberately match more than one physical modifier set, and
/// the predicates are exactly the ones the old dispatch cascade used — a
/// binding must not become reachable or unreachable by moving into the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mods {
    /// [`key::belongs_to_desktop`]: Super/Command, the modifier no shell can
    /// receive. Shifted keys arrive pre-shifted (`{`, not `⇧[`).
    Desktop,
    /// [`key::is_clipboard_chord`]: Super *or* Ctrl+Shift, both, everywhere.
    Clipboard,
    /// Ctrl without Shift.
    Ctrl,
    /// Ctrl with Shift. Kept disjoint from [`Mods::Ctrl`] so two rows on the
    /// same key cannot both match one press.
    CtrlShift,
    /// Shift, nothing excluded — the old scrollback check tested only
    /// `shift_key()`, so Ctrl+Shift+PgUp paged too, and behavior-preserving
    /// means keeping that.
    Shift,
}

/// The key half of a chord, stored as winit *delivers* it, never as the
/// physical keycap: ⌘⇧[ arrives as `Character("{")`, Ctrl+Tab as
/// `Named(Tab)`. This is layout-dependent — on a layout where ⇧[ is not `{`
/// these chords are unreachable, exactly as they were before the table;
/// fixing that means physical-key matching and belongs to the rebinding
/// milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordKey {
    Char(&'static str),
    Named(NamedKey),
}

/// Context gate for bindings that are conditional but not modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// In the alternate screen the chord must *fall through* to the pty
    /// encoder, not be swallowed: `less` and `vim` page themselves and are
    /// owed the bytes.
    NotAltScreen,
}

/// Where a binding appears in the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Tabs,
    Fleet,
    Clipboard,
    Blocks,
    Scrollback,
    Help,
}

pub struct Binding {
    pub mods: Mods,
    pub key: ChordKey,
    pub action: Action,
    pub when: When,
    /// The human keycap for display, decoupled from the match form: the `{`
    /// binding shows as `⇧[`, `Named(Tab)` as `Tab`.
    pub keycap: &'static str,
    pub name: &'static str,
    pub category: Category,
    /// Alias rows (`?` for `/`, `⇧P` for the palette) get no palette line of
    /// their own; the canonical binding shows the chord.
    pub show: bool,
}

const fn b(
    mods: Mods,
    key: ChordKey,
    action: Action,
    keycap: &'static str,
    name: &'static str,
    category: Category,
) -> Binding {
    Binding { mods, key, action, when: When::Always, keycap, name, category, show: true }
}

const fn hidden(mods: Mods, key: ChordKey, action: Action, category: Category) -> Binding {
    Binding { mods, key, action, when: When::Always, keycap: "", name: "", category, show: false }
}

const fn scroll(key: ChordKey, action: Action, keycap: &'static str, name: &'static str) -> Binding {
    Binding {
        mods: Mods::Shift,
        key,
        action,
        when: When::NotAltScreen,
        keycap,
        name,
        category: Category::Scrollback,
        show: true,
    }
}

/// Order matters: first match wins, which is the precedence the old if-cascade
/// had by source order. In particular the Desktop rows sit above the Clipboard
/// rows because on macOS ⌘ satisfies both policies — the desktop letters are
/// simply not c/v/o/r, so nothing shadows, but the ordering keeps that true by
/// construction rather than by inspection.
pub static BINDINGS: &[Binding] = &[
    // Desktop chords. Lowercase-exact on purpose: shift produces the
    // uppercase, and ⌘⇧T stays reserved (reopen-closed, one day).
    b(Mods::Desktop, ChordKey::Char("t"), Action::NewTab, "T", "New tab", Category::Tabs),
    b(
        Mods::Desktop,
        ChordKey::Char("k"),
        Action::ToggleFleetPicker,
        "K",
        "Fleet picker",
        Category::Fleet,
    ),
    b(Mods::Desktop, ChordKey::Char("w"), Action::CloseTab, "W", "Close tab", Category::Tabs),
    // Each digit is its own palette command — "go to tab 3" must be
    // searchable and runnable, not a footnote of "1–8".
    b(Mods::Desktop, ChordKey::Char("1"), Action::ActivateTab(0), "1", "Go to tab 1", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("2"), Action::ActivateTab(1), "2", "Go to tab 2", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("3"), Action::ActivateTab(2), "3", "Go to tab 3", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("4"), Action::ActivateTab(3), "4", "Go to tab 4", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("5"), Action::ActivateTab(4), "5", "Go to tab 5", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("6"), Action::ActivateTab(5), "6", "Go to tab 6", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("7"), Action::ActivateTab(6), "7", "Go to tab 7", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("8"), Action::ActivateTab(7), "8", "Go to tab 8", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("9"), Action::ActivateLastTab, "9", "Go to last tab", Category::Tabs),
    // ⌘⇧[ and ⌘⇧] arrive as { and }; the keycap shows the physical key.
    b(Mods::Desktop, ChordKey::Char("{"), Action::PrevTab, "⇧[", "Previous tab", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("}"), Action::NextTab, "⇧]", "Next tab", Category::Tabs),
    // Ctrl+Tab / Ctrl+Shift+Tab cycle, as in every tabbed app.
    b(Mods::Ctrl, ChordKey::Named(NamedKey::Tab), Action::NextTab, "Tab", "Next tab", Category::Tabs),
    b(
        Mods::CtrlShift,
        ChordKey::Named(NamedKey::Tab),
        Action::PrevTab,
        "Tab",
        "Previous tab",
        Category::Tabs,
    ),
    // The clipboard family. Blocks (o/r) share the chord because they are the
    // same kind of thing — the desktop acting on the terminal — and because
    // it is the chord the encoder already refuses to pass to the shell.
    b(Mods::Clipboard, ChordKey::Char("c"), Action::Copy, "C", "Copy", Category::Clipboard),
    b(Mods::Clipboard, ChordKey::Char("v"), Action::Paste, "V", "Paste", Category::Clipboard),
    b(
        Mods::Clipboard,
        ChordKey::Char("o"),
        Action::CopyBlockOutput,
        "O",
        "Copy last command's output",
        Category::Blocks,
    ),
    b(
        Mods::Clipboard,
        ChordKey::Char("r"),
        Action::RerunLastCommand,
        "R",
        "Re-run last command",
        Category::Blocks,
    ),
    // Scrollback paging. The shift is what makes it unambiguous: bare PgUp
    // still belongs to the program.
    scroll(ChordKey::Named(NamedKey::PageUp), Action::ScrollPageUp, "PgUp", "Page up"),
    scroll(ChordKey::Named(NamedKey::PageDown), Action::ScrollPageDown, "PgDn", "Page down"),
    // The palette itself. Four chords, one visible row. ⌘P is canonical —
    // it is what fingers actually try, and it was a dead chord anyway: the
    // desktop modifier never reaches the shell, so nothing is lost. ⌘⇧P is
    // the spelling the editors taught; ⌘/ and ⌘? (also Ctrl+Shift+/, which
    // arrives as "?") are the help-key tradition.
    b(Mods::Desktop, ChordKey::Char("p"), Action::TogglePalette, "P", "Command palette", Category::Help),
    hidden(Mods::Desktop, ChordKey::Char("P"), Action::TogglePalette, Category::Help),
    hidden(Mods::Clipboard, ChordKey::Char("/"), Action::TogglePalette, Category::Help),
    hidden(Mods::Clipboard, ChordKey::Char("?"), Action::TogglePalette, Category::Help),
    // ⌘, — the settings chord every desktop app shares.
    b(Mods::Desktop, ChordKey::Char(","), Action::ToggleSettings, ",", "Settings", Category::Help),
];

fn mods_match(m: Mods, s: ModifiersState) -> bool {
    match m {
        Mods::Desktop => key::belongs_to_desktop(s),
        Mods::Clipboard => key::is_clipboard_chord(s),
        Mods::Ctrl => s.control_key() && !s.shift_key(),
        Mods::CtrlShift => s.control_key() && s.shift_key(),
        Mods::Shift => s.shift_key(),
    }
}

fn key_match(binding: &Binding, logical: &Key) -> bool {
    match (&binding.key, logical) {
        (ChordKey::Named(want), Key::Named(got)) => want == got,
        (ChordKey::Char(want), Key::Character(got)) => match binding.mods {
            // Ctrl+Shift+C arrives uppercase; the old clipboard block
            // lowercased before matching.
            Mods::Clipboard => got.eq_ignore_ascii_case(want),
            // Desktop is exact: ⌘⇧T must not be ⌘T.
            _ => got.as_str() == *want,
        },
        _ => false,
    }
}

/// First match in table order — table order is the dispatch precedence.
#[must_use]
pub fn lookup(logical: &Key, mods: ModifiersState) -> Option<&'static Binding> {
    BINDINGS.iter().find(|binding| mods_match(binding.mods, mods) && key_match(binding, logical))
}

const MAC: bool = cfg!(target_os = "macos");

/// The platform-primary spelling of a chord.
///
/// The clipboard policy is *both* chords everywhere; the sheet shows the
/// local convention and one section note says the other is accepted too —
/// that fact lives in one place, next to `key::is_clipboard_chord`.
/// `Mods::Desktop` renders honestly as Super off macOS, because
/// `belongs_to_desktop` is super-only on every platform; if the tab chords
/// grow a Ctrl+Shift form later, that is a policy change and this label
/// updates itself.
#[must_use]
pub fn chord_label(binding: &Binding) -> String {
    let prefix = match binding.mods {
        Mods::Desktop => {
            if MAC {
                "⌘"
            } else {
                "Super+"
            }
        }
        Mods::Clipboard => {
            if MAC {
                "⌘"
            } else {
                "Ctrl+Shift+"
            }
        }
        Mods::Ctrl => {
            if MAC {
                "⌃"
            } else {
                "Ctrl+"
            }
        }
        Mods::CtrlShift => {
            if MAC {
                "⌃⇧"
            } else {
                "Ctrl+Shift+"
            }
        }
        Mods::Shift => {
            if MAC {
                "⇧"
            } else {
                "Shift+"
            }
        }
    };
    format!("{prefix}{}", binding.keycap)
}

/// A pointer chord, listed beside the keyboard ones.
///
/// These cannot live in [`BINDINGS`] — there is no `Key` to match — but the
/// file that names every chord should name all of them; the handlers live in
/// `app.rs`'s mouse arms and this list is their public face.
pub struct MouseShortcut {
    pub gesture: &'static str,
    pub name: &'static str,
}

pub static MOUSE_SHORTCUTS: &[MouseShortcut] = &[
    MouseShortcut {
        gesture: if MAC { "⌘ Click" } else { "Ctrl+Shift+Click" },
        name: "Copy that command's output",
    },
    MouseShortcut {
        gesture: "Shift+Click / Drag",
        name: "Select even when the program owns the mouse",
    },
    MouseShortcut { gesture: if MAC { "⌥ Drag" } else { "Alt+Drag" }, name: "Rectangular selection" },
    MouseShortcut { gesture: "Middle-click", name: "Paste" },
    MouseShortcut { gesture: "Right-click", name: "Copy the selection, else paste" },
    MouseShortcut { gesture: "Double-click title bar", name: "Zoom the window" },
];

const CATEGORY_ORDER: &[(Category, &str)] = &[
    (Category::Tabs, "Tabs"),
    (Category::Fleet, "Fleet"),
    (Category::Clipboard, "Copy & paste"),
    (Category::Blocks, "Command blocks"),
    (Category::Scrollback, "Scrollback"),
    (Category::Help, "Help"),
];

fn row_matches(filter: &str, name: &str, chord: &str) -> bool {
    filter.is_empty()
        || name.to_lowercase().contains(filter)
        || chord.to_lowercase().contains(filter)
}

/// The command palette's rows and the actions they run — parallel lists,
/// one pass (the picker discipline: index `n` means the same thing to the
/// renderer and the input path by construction).
///
/// Every row shows its chord and Enter runs its action through the same
/// dispatch the chord uses, so "what it says" and "what it does" are one
/// fact. Reference rows carry `None` and the selection skips them: mouse
/// gestures (there is no `Key` to replay), the both-conventions footnote,
/// and the palette's own entry (running "open the palette" from inside it
/// is a no-op wearing a command's name). A command with no chord at all
/// becomes representable the day one exists — this list, not [`BINDINGS`],
/// is the palette's contract.
#[must_use]
pub fn palette(filter: &str) -> (Vec<PaletteRow>, Vec<Option<Action>>) {
    let filter = filter.to_lowercase();
    let mut rows = Vec::new();
    let mut actions = Vec::new();

    for (category, title) in CATEGORY_ORDER {
        let start = rows.len();
        for binding in BINDINGS.iter().filter(|b| b.show && b.category == *category) {
            let chord = chord_label(binding);
            if !row_matches(&filter, binding.name, &chord) {
                continue;
            }
            let runnable = binding.action != Action::TogglePalette;
            rows.push(PaletteRow::Command {
                name: binding.name.to_string(),
                chord,
                runnable,
            });
            actions.push(runnable.then_some(binding.action));
        }
        if rows.len() == start {
            continue;
        }
        rows.insert(start, PaletteRow::Group { title: (*title).to_string() });
        actions.insert(start, None);
        if *category == Category::Clipboard {
            // The one place the both-conventions fact is displayed; see
            // `key::is_clipboard_chord` for why both are accepted.
            rows.push(PaletteRow::Command {
                name: "⌘ and Ctrl+Shift both work, on every platform".to_string(),
                chord: String::new(),
                runnable: false,
            });
            actions.push(None);
        }
    }

    let start = rows.len();
    for shortcut in MOUSE_SHORTCUTS {
        if !row_matches(&filter, shortcut.name, shortcut.gesture) {
            continue;
        }
        rows.push(PaletteRow::Command {
            name: shortcut.name.to_string(),
            chord: shortcut.gesture.to_string(),
            runnable: false,
        });
        actions.push(None);
    }
    if rows.len() > start {
        rows.insert(start, PaletteRow::Group { title: "Mouse".to_string() });
        actions.insert(start, None);
    }

    (rows, actions)
}

/// The nearest runnable row at or after `from`, wrapping backwards —
/// headers and reference rows are labels; the keyboard never rests on one.
#[must_use]
pub fn nearest_runnable(actions: &[Option<Action>], from: usize) -> usize {
    let runnable = |i: &usize| actions.get(*i).copied().flatten().is_some();
    (from..actions.len())
        .find(runnable)
        .or_else(|| (0..from).rev().find(runnable))
        .unwrap_or(0)
}

/// The next runnable row in the given direction, or `from` at the edge.
#[must_use]
pub fn step_runnable(actions: &[Option<Action>], from: usize, down: bool) -> usize {
    let runnable = |i: &usize| actions.get(*i).copied().flatten().is_some();
    if down {
        (from + 1..actions.len()).find(runnable).unwrap_or(from)
    } else {
        (0..from).rev().find(runnable).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_key(s: &str) -> Key {
        Key::Character(s.into())
    }

    fn action_for(logical: &Key, mods: ModifiersState) -> Option<Action> {
        lookup(logical, mods).map(|b| b.action)
    }

    const SUPER: ModifiersState = ModifiersState::SUPER;
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;

    #[test]
    fn the_table_resolves_every_chord_the_cascade_did() {
        // A pinned copy of the old if-cascade's behavior, including the
        // arrivals that are easy to get wrong. If a refactor of the table
        // changes any line of this, it changed a shortcut.
        let pinned: &[(Key, ModifiersState, Action)] = &[
            (char_key("t"), SUPER, Action::NewTab),
            (char_key("k"), SUPER, Action::ToggleFleetPicker),
            (char_key("w"), SUPER, Action::CloseTab),
            (char_key("1"), SUPER, Action::ActivateTab(0)),
            (char_key("8"), SUPER, Action::ActivateTab(7)),
            (char_key("9"), SUPER, Action::ActivateLastTab),
            // ⌘⇧[ arrives pre-shifted; the table stores winit's delivery,
            // not the keycap.
            (char_key("{"), SUPER.union(SHIFT), Action::PrevTab),
            (char_key("}"), SUPER.union(SHIFT), Action::NextTab),
            (Key::Named(NamedKey::Tab), CTRL, Action::NextTab),
            (Key::Named(NamedKey::Tab), CTRL.union(SHIFT), Action::PrevTab),
            // Ctrl+Tab never excluded Super in the old cascade.
            (Key::Named(NamedKey::Tab), CTRL.union(SUPER), Action::NextTab),
            (char_key("c"), SUPER, Action::Copy),
            // Ctrl+Shift+C arrives uppercase.
            (char_key("C"), CTRL.union(SHIFT), Action::Copy),
            // ...and ⌘⇧C copied in the old cascade too: the desktop block had
            // no "C" arm, so it fell through to the case-folding clipboard
            // block.
            (char_key("C"), SUPER.union(SHIFT), Action::Copy),
            (char_key("v"), SUPER, Action::Paste),
            (char_key("V"), CTRL.union(SHIFT), Action::Paste),
            (char_key("o"), SUPER, Action::CopyBlockOutput),
            (char_key("r"), SUPER, Action::RerunLastCommand),
            // The scroll rows match any shift chord, as the old check did —
            // Ctrl+Shift+PgUp paged then and must page now. (The alt-screen
            // fall-through is the dispatcher's job, not the table's.)
            (Key::Named(NamedKey::PageUp), SHIFT, Action::ScrollPageUp),
            (Key::Named(NamedKey::PageUp), CTRL.union(SHIFT), Action::ScrollPageUp),
            (Key::Named(NamedKey::PageDown), SHIFT, Action::ScrollPageDown),
        ];
        for (logical, mods, want) in pinned {
            assert_eq!(
                action_for(logical, *mods),
                Some(*want),
                "{logical:?} + {mods:?} must resolve to {want:?}"
            );
        }
    }

    #[test]
    fn reserved_and_bare_keys_never_match() {
        assert_eq!(
            action_for(&char_key("T"), SUPER.union(SHIFT)),
            None,
            "⌘⇧T is reserved for reopen-closed-tab; matching it as ⌘T would burn the slot"
        );
        for binding in BINDINGS {
            if let ChordKey::Char(c) = binding.key {
                assert_eq!(
                    action_for(&char_key(c), ModifiersState::empty()),
                    None,
                    "bare '{c}' must reach the shell, not the app — otherwise typing it acts"
                );
            }
        }
        assert_eq!(
            action_for(&char_key("t"), CTRL),
            None,
            "Ctrl+T belongs to the shell (it is not the clipboard chord without shift)"
        );
        assert_eq!(
            action_for(&Key::Named(NamedKey::PageUp), ModifiersState::empty()),
            None,
            "bare PgUp still belongs to the program"
        );
    }

    #[test]
    fn the_palette_chord_and_its_aliases_all_resolve() {
        assert_eq!(action_for(&char_key("/"), SUPER), Some(Action::TogglePalette));
        assert_eq!(
            action_for(&char_key("?"), SUPER),
            Some(Action::TogglePalette),
            "⌘? arrives pre-shifted as ?"
        );
        assert_eq!(
            action_for(&char_key("?"), CTRL.union(SHIFT)),
            Some(Action::TogglePalette),
            "Ctrl+Shift+/ arrives as ?, and case-folding cannot map ? to / for it"
        );
        assert_eq!(
            action_for(&char_key("P"), SUPER.union(SHIFT)),
            Some(Action::TogglePalette),
            "⌘⇧P is the spelling every editor taught"
        );
        assert_eq!(
            action_for(&char_key("p"), SUPER),
            Some(Action::TogglePalette),
            "⌘P is what fingers actually try, and it was a dead chord before"
        );
    }

    #[test]
    fn every_visible_binding_is_a_palette_row_that_runs_its_own_action() {
        // "Trigger and show" must be one fact: the row that displays a
        // chord runs exactly the action that chord dispatches to, verified
        // through the same parallel-list the input path uses.
        let (rows, actions) = palette("");
        for binding in BINDINGS.iter().filter(|b| b.show) {
            let at = rows
                .iter()
                .position(|r| matches!(r, PaletteRow::Command { name, chord, .. }
                    if *name == binding.name && *chord == chord_label(binding)))
                .unwrap_or_else(|| {
                    panic!(
                        "'{}' works but is not in the palette — the drift this table exists to kill",
                        binding.name
                    )
                });
            let expected = (binding.action != Action::TogglePalette).then_some(binding.action);
            assert_eq!(
                actions[at], expected,
                "'{}': the palette must run what the chord runs",
                binding.name
            );
        }
        // Alias rows are collapsed, not secret: a visible row must perform
        // the same kind of action, or a chord became undiscoverable.
        for hidden in BINDINGS.iter().filter(|b| !b.show) {
            assert!(
                BINDINGS.iter().any(|v| v.show
                    && std::mem::discriminant(&v.action) == std::mem::discriminant(&hidden.action)),
                "a hidden binding for {:?} has no visible representative",
                hidden.action
            );
        }
        assert!(
            rows.iter().any(|r| matches!(r, PaletteRow::Group { title } if title == "Mouse")),
            "the pointer chords must be listed too — they exist nowhere else"
        );
    }

    #[test]
    fn filtering_never_leaves_an_empty_group() {
        for filter in ["tab", "copy", "PASTE", "no-such-command-anywhere"] {
            let (rows, actions) = palette(filter);
            assert_eq!(rows.len(), actions.len(), "the lists are parallel by construction");
            for (i, row) in rows.iter().enumerate() {
                if matches!(row, PaletteRow::Group { .. }) {
                    assert!(
                        matches!(rows.get(i + 1), Some(PaletteRow::Command { .. })),
                        "'{filter}' left a group header with nothing under it"
                    );
                }
            }
        }
        let (rows, _) = palette("PASTE");
        assert!(
            rows.iter().any(|r| matches!(r, PaletteRow::Command { name, .. } if name == "Paste")),
            "the filter must be case-insensitive"
        );
    }

    #[test]
    fn selection_skips_headers_and_reference_rows() {
        let (rows, actions) = palette("");
        let first = nearest_runnable(&actions, 0);
        assert!(actions[first].is_some(), "the selection must land on a runnable command");
        assert!(
            matches!(&rows[0], PaletteRow::Group { .. }),
            "…and row zero is a header, so landing there would be resting on a label"
        );
        // Walking down from the last runnable row goes nowhere.
        let last = (0..actions.len()).rev().find(|i| actions[*i].is_some()).expect("some");
        assert_eq!(step_runnable(&actions, last, true), last, "the end is a wall, not a wrap");
    }

    #[test]
    fn chord_labels_use_the_platform_convention() {
        let brace = BINDINGS
            .iter()
            .find(|b| matches!(b.key, ChordKey::Char("{")))
            .expect("the previous-tab binding exists");
        let label = chord_label(brace);
        assert!(
            label.contains('[') && !label.contains('{'),
            "the sheet shows the physical keycap, not winit's shifted delivery: {label}"
        );
    }
}
