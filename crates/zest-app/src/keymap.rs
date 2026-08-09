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

pub struct Binding {
    pub mods: Mods,
    pub key: ChordKey,
    pub action: Action,
    pub when: When,
}

const fn b(mods: Mods, key: ChordKey, action: Action) -> Binding {
    Binding { mods, key, action, when: When::Always }
}

const fn when(mods: Mods, key: ChordKey, action: Action, when: When) -> Binding {
    Binding { mods, key, action, when }
}

/// Order matters: first match wins, which is the precedence the old if-cascade
/// had by source order. In particular the Desktop rows sit above the Clipboard
/// rows because on macOS ⌘ satisfies both policies — the desktop letters are
/// simply not c/v/o/r, so nothing shadows, but the ordering keeps that true by
/// construction rather than by inspection.
pub static BINDINGS: &[Binding] = &[
    // Desktop chords. Lowercase-exact on purpose: shift produces the
    // uppercase, and ⌘⇧T stays reserved (reopen-closed, one day).
    b(Mods::Desktop, ChordKey::Char("t"), Action::NewTab),
    b(Mods::Desktop, ChordKey::Char("k"), Action::ToggleFleetPicker),
    b(Mods::Desktop, ChordKey::Char("w"), Action::CloseTab),
    b(Mods::Desktop, ChordKey::Char("1"), Action::ActivateTab(0)),
    b(Mods::Desktop, ChordKey::Char("2"), Action::ActivateTab(1)),
    b(Mods::Desktop, ChordKey::Char("3"), Action::ActivateTab(2)),
    b(Mods::Desktop, ChordKey::Char("4"), Action::ActivateTab(3)),
    b(Mods::Desktop, ChordKey::Char("5"), Action::ActivateTab(4)),
    b(Mods::Desktop, ChordKey::Char("6"), Action::ActivateTab(5)),
    b(Mods::Desktop, ChordKey::Char("7"), Action::ActivateTab(6)),
    b(Mods::Desktop, ChordKey::Char("8"), Action::ActivateTab(7)),
    b(Mods::Desktop, ChordKey::Char("9"), Action::ActivateLastTab),
    // ⌘⇧[ and ⌘⇧] arrive as { and }.
    b(Mods::Desktop, ChordKey::Char("{"), Action::PrevTab),
    b(Mods::Desktop, ChordKey::Char("}"), Action::NextTab),
    // Ctrl+Tab / Ctrl+Shift+Tab cycle, as in every tabbed app.
    b(Mods::Ctrl, ChordKey::Named(NamedKey::Tab), Action::NextTab),
    b(Mods::CtrlShift, ChordKey::Named(NamedKey::Tab), Action::PrevTab),
    // The clipboard family. Blocks (o/r) share the chord because they are the
    // same kind of thing — the desktop acting on the terminal — and because
    // it is the chord the encoder already refuses to pass to the shell.
    b(Mods::Clipboard, ChordKey::Char("c"), Action::Copy),
    b(Mods::Clipboard, ChordKey::Char("v"), Action::Paste),
    b(Mods::Clipboard, ChordKey::Char("o"), Action::CopyBlockOutput),
    b(Mods::Clipboard, ChordKey::Char("r"), Action::RerunLastCommand),
    // Scrollback paging. The shift is what makes it unambiguous: bare PgUp
    // still belongs to the program.
    when(Mods::Shift, ChordKey::Named(NamedKey::PageUp), Action::ScrollPageUp, When::NotAltScreen),
    when(
        Mods::Shift,
        ChordKey::Named(NamedKey::PageDown),
        Action::ScrollPageDown,
        When::NotAltScreen,
    ),
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
}
