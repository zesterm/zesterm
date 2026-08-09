//! Every chord the app owns, in one table.
//!
//! Before this table, shortcuts were unnamed if-blocks in `window_event` — a
//! shape that cannot be enumerated, so nothing could *display* the shortcuts
//! without hand-maintaining a second list that drifts the first time anyone
//! adds a chord. The table is what the dispatch consults and what the
//! shortcuts sheet renders, so an unlisted chord structurally cannot exist.
//! It is also the rail user-configurable keybindings would layer onto: a
//! config section becomes data merged over [`BINDINGS`], not a rewrite.
//!
//! Modal overlays (the picker's type-to-filter, for instance) stay outside the
//! table on purpose: their keys are a line editor, not commands, and "any
//! character appends to the filter" is not expressible as a chord row.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use zest_input::key;

use crate::chrome::model::{ShortcutRow, ShortcutSection};

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
    /// The shortcuts sheet itself — rendered from this table, so it can
    /// never list a chord that does not exist.
    ToggleShortcuts,
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

/// Where a binding appears on the shortcuts sheet.
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
    /// Collapsed rows (⌘2..⌘8, the `?` alias of `/`) get no line of their
    /// own on the sheet; the representative row shows the range.
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
    b(Mods::Desktop, ChordKey::Char("1"), Action::ActivateTab(0), "1…8", "Go to tab 1–8", Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("2"), Action::ActivateTab(1), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("3"), Action::ActivateTab(2), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("4"), Action::ActivateTab(3), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("5"), Action::ActivateTab(4), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("6"), Action::ActivateTab(5), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("7"), Action::ActivateTab(6), Category::Tabs),
    hidden(Mods::Desktop, ChordKey::Char("8"), Action::ActivateTab(7), Category::Tabs),
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
    // The sheet itself. Two rows because ⌘/ arrives as "/" while ⌘? and
    // Ctrl+Shift+/ arrive as "?" — one visible row covers both.
    b(
        Mods::Clipboard,
        ChordKey::Char("/"),
        Action::ToggleShortcuts,
        "/",
        "Keyboard shortcuts",
        Category::Help,
    ),
    hidden(Mods::Clipboard, ChordKey::Char("?"), Action::ToggleShortcuts, Category::Help),
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

const SECTION_ORDER: &[(Category, &str)] = &[
    (Category::Tabs, "Tabs"),
    (Category::Fleet, "Fleet"),
    (Category::Clipboard, "Copy & paste"),
    (Category::Blocks, "Command blocks"),
    (Category::Scrollback, "Scrollback"),
    (Category::Help, "Help"),
];

fn row_matches(filter: &str, row: &ShortcutRow) -> bool {
    filter.is_empty()
        || row.name.to_lowercase().contains(filter)
        || row.chord.to_lowercase().contains(filter)
}

/// The shortcuts sheet's content, straight from the table.
///
/// Pure so it is testable without a window; the app calls it from
/// `refresh_chrome` with the live filter.
#[must_use]
pub fn sections(filter: &str) -> Vec<ShortcutSection> {
    let filter = filter.to_lowercase();
    let mut out = Vec::new();
    for (category, title) in SECTION_ORDER {
        let rows: Vec<ShortcutRow> = BINDINGS
            .iter()
            .filter(|binding| binding.show && binding.category == *category)
            .map(|binding| ShortcutRow {
                name: binding.name.to_string(),
                chord: chord_label(binding),
            })
            .filter(|row| row_matches(&filter, row))
            .collect();
        if rows.is_empty() {
            continue;
        }
        // The one place the both-conventions fact is displayed; see
        // `key::is_clipboard_chord` for why both are accepted.
        let note = (*category == Category::Clipboard)
            .then(|| "⌘ and Ctrl+Shift both work, on every platform".to_string());
        out.push(ShortcutSection { title: (*title).to_string(), rows, note });
    }
    let mouse: Vec<ShortcutRow> = MOUSE_SHORTCUTS
        .iter()
        .map(|shortcut| ShortcutRow {
            name: shortcut.name.to_string(),
            chord: shortcut.gesture.to_string(),
        })
        .filter(|row| row_matches(&filter, row))
        .collect();
    if !mouse.is_empty() {
        out.push(ShortcutSection { title: "Mouse".to_string(), rows: mouse, note: None });
    }
    out
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
    fn the_sheet_chord_and_its_aliases_all_resolve() {
        assert_eq!(action_for(&char_key("/"), SUPER), Some(Action::ToggleShortcuts));
        assert_eq!(
            action_for(&char_key("?"), SUPER),
            Some(Action::ToggleShortcuts),
            "⌘? arrives pre-shifted as ?"
        );
        assert_eq!(
            action_for(&char_key("?"), CTRL.union(SHIFT)),
            Some(Action::ToggleShortcuts),
            "Ctrl+Shift+/ arrives as ?, and case-folding cannot map ? to / for it"
        );
    }

    #[test]
    fn every_visible_binding_reaches_the_cheat_sheet() {
        let secs = sections("");
        let rows: Vec<&ShortcutRow> = secs.iter().flat_map(|s| s.rows.iter()).collect();
        for binding in BINDINGS.iter().filter(|b| b.show) {
            assert!(
                rows.iter()
                    .any(|r| r.name == binding.name && r.chord == chord_label(binding)),
                "'{}' works but is not on the sheet — the drift this table exists to kill",
                binding.name
            );
        }
        // Hidden rows are collapsed, not secret: a visible row must perform
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
            secs.iter().any(|s| s.title == "Mouse" && !s.rows.is_empty()),
            "the pointer chords must be listed too — they exist nowhere else"
        );
    }

    #[test]
    fn filtering_never_leaves_an_empty_section() {
        for filter in ["tab", "copy", "PASTE", "no-such-shortcut-anywhere"] {
            for section in sections(filter) {
                assert!(
                    !section.rows.is_empty(),
                    "'{filter}' left section '{}' as an empty header",
                    section.title
                );
            }
        }
        assert!(
            sections("PASTE").iter().any(|s| s.rows.iter().any(|r| r.name == "Paste")),
            "the filter must be case-insensitive"
        );
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
