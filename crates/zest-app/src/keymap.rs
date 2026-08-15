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

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

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
    /// The Settings tab (⌘, — the desktop's own convention). Opens it, or
    /// activates the one that exists; closing it is closing a tab (§11).
    ToggleSettings,
    /// The Profiles tab (⌘⇧, — the settings chord, shifted, per design §12).
    /// Opens the singleton tab, or activates it when it is already open.
    OpenProfiles,
    /// Horizontal ⇄ vertical tabs (⌘⇧E, and the title bar's pill). Writes
    /// `tabs.position` through the settings path, so the file stays the one
    /// source of truth.
    ToggleTabLayout,
    /// Split the active tab right (⌘D); on an already-split tab, moves the
    /// keyboard to the other pane.
    SplitRight,
}

/// The modifier half of a chord, as *policy* rather than bitmask.
///
/// Two of these deliberately match more than one physical modifier set, and
/// the predicates are exactly the ones the old dispatch cascade used — a
/// binding must not become reachable or unreachable by moving into the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mods {
    /// Super/Command *or* Ctrl+Shift, both accepted everywhere — the same
    /// policy as [`Mods::Clipboard`], and for the same reason.
    ///
    /// Super alone was the rule until Windows was looked at: the shell there
    /// reserves Win+T, Win+W, Win+K, Win+P, Win+, and Win+1–9, so every chord
    /// in this family was unreachable on the platform the project calls
    /// primary. Ctrl+Shift is what Windows Terminal and VS Code use and what
    /// the clipboard rows had already settled on.
    ///
    /// Note this is *not* [`zest_input::key::belongs_to_desktop`], which stayed super-only
    /// on purpose: that predicate is the pty encoder's gate, and widening it
    /// would stop Ctrl+Shift+Arrow, the Ctrl+Shift F-keys and vim's `CTRL-^`
    /// from reaching the shell at all. The policy moved; the encoder did not.
    ///
    /// Shifted keys arrive pre-shifted (`{`, not `⇧[`).
    Desktop,
    /// [`zest_input::key::is_clipboard_chord`]: Super *or* Ctrl+Shift, both, everywhere.
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
    /// Super *with* Shift required — for a key whose unshifted chord is
    /// already spoken for (⌘, is Settings, ⌘⇧, is Profiles). The Ctrl+Shift
    /// convention structurally cannot spell it: Shift is spent on the
    /// modifier, so Ctrl+Shift+<key> stays with the unshifted sibling. A row
    /// in this family is therefore mac-only from the keyboard and MUST keep
    /// a palette row and a clickable affordance; `chord_label` prints
    /// nothing off macOS rather than naming a chord that runs something else.
    SuperShift,
}

/// The key half of a chord, stored as winit *delivers* it, never as the
/// physical keycap: ⌘⇧[ arrives as `Character("{")`, Ctrl+Tab as
/// `Named(Tab)`. This is layout-dependent — on a layout where ⇧[ is not `{`
/// those chords are unreachable, exactly as they were before the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordKey {
    Char(&'static str),
    Named(NamedKey),
    /// Matched by *position* rather than by what it types.
    ///
    /// For chords whose keycap becomes unreachable once Shift is spent on the
    /// modifier: ⌘1 arrives as `Character("1")`, but Ctrl+Shift+1 arrives as
    /// the shifted symbol — `!` on US, and the row differs again on every
    /// other layout (Swedish gives `! " # ¤ % & / ( )`). There is no character
    /// to write in the table, so the table names the key instead.
    ///
    /// It fixes a live bug on the way past: ⌘1 does nothing on a French Mac
    /// today, because the digit row there types `&é"'(`.
    Code(KeyCode),
}

/// Which spelling of a two-convention policy a press actually used.
///
/// The distinction is load-bearing rather than bookkeeping. Ctrl+Shift spends
/// Shift on the modifier, so the letter arrives *uppercase* and has to be
/// folded; the Super form must stay exact, or ⌘⇧T matches the ⌘T row and burns
/// the slot reopen-closed-tab is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Super,
    CtrlShift,
    /// Neither convention — a plain Ctrl, Ctrl+Shift or Shift row, where the
    /// question does not arise.
    Plain,
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
    // searchable and runnable, not a footnote of "1–8". Matched by position:
    // see `ChordKey::Code` for why a digit is the one thing that cannot be
    // written as the character it types.
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit1), Action::ActivateTab(0), "1", "Go to tab 1", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit2), Action::ActivateTab(1), "2", "Go to tab 2", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit3), Action::ActivateTab(2), "3", "Go to tab 3", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit4), Action::ActivateTab(3), "4", "Go to tab 4", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit5), Action::ActivateTab(4), "5", "Go to tab 5", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit6), Action::ActivateTab(5), "6", "Go to tab 6", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit7), Action::ActivateTab(6), "7", "Go to tab 7", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit8), Action::ActivateTab(7), "8", "Go to tab 8", Category::Tabs),
    b(Mods::Desktop, ChordKey::Code(KeyCode::Digit9), Action::ActivateLastTab, "9", "Go to last tab", Category::Tabs),
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
        "Copy the block's output",
        Category::Blocks,
    ),
    b(
        Mods::Clipboard,
        ChordKey::Char("r"),
        Action::RerunLastCommand,
        "R",
        "Re-run the block",
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
    // ⌘⇧, — the Profiles tab (design §12), BEFORE the settings row because
    // the Desktop policy ignores Shift under Super: with the order flipped,
    // ⌘⇧, would match the settings row first and Profiles would be one more
    // dead design chord. On Windows both spellings collapse onto
    // Ctrl+Shift+, — which stays with Settings, the unshifted meaning — so
    // this row is mac-only from the keyboard (see `Mods::SuperShift`); the
    // palette row and the launcher's "Manage profiles" row carry it there.
    b(Mods::SuperShift, ChordKey::Code(KeyCode::Comma), Action::OpenProfiles, "⇧,", "Open profiles", Category::Help),
    // ⌘, — the settings chord every desktop app shares. By position for the
    // same reason as the digits: Shift+, is `<`, so the Ctrl+Shift form has no
    // comma in it.
    b(Mods::Desktop, ChordKey::Code(KeyCode::Comma), Action::ToggleSettings, ",", "Settings", Category::Help),
    // ⌘⇧E arrives as "E" (shift pre-applies, like { above); the keycap shows
    // the physical chord.
    b(Mods::Desktop, ChordKey::Char("E"), Action::ToggleTabLayout, "⇧E", "Toggle vertical tabs", Category::Tabs),
    b(Mods::Desktop, ChordKey::Char("d"), Action::SplitRight, "D", "Split right", Category::Tabs),
];

/// The platform-spelled chord of `action`'s first binding — what the title
/// bar's pills print. Empty when the action has no chord, which the caller
/// treats as "draw no chip" rather than an error.
#[must_use]
pub fn chord_for(action: Action) -> String {
    BINDINGS.iter().find(|b| b.action == action).map(chord_label).unwrap_or_default()
}

/// Which convention this press used for `m`, or `None` if it is not that row.
fn mods_match(m: Mods, s: ModifiersState) -> Option<Form> {
    // Super first in both two-convention arms: on macOS ⌘ satisfies either
    // spelling, and answering `Super` there is what keeps ⌘⇧T exact.
    let two_conventions = || {
        if s.super_key() {
            Some(Form::Super)
        } else if s.control_key() && s.shift_key() {
            Some(Form::CtrlShift)
        } else {
            None
        }
    };
    match m {
        Mods::Desktop | Mods::Clipboard => two_conventions(),
        Mods::Ctrl => (s.control_key() && !s.shift_key()).then_some(Form::Plain),
        Mods::CtrlShift => (s.control_key() && s.shift_key()).then_some(Form::Plain),
        Mods::Shift => s.shift_key().then_some(Form::Plain),
        // Guarded to macOS: on Windows `super` is the Win key, so Win+Shift+,
        // would run an action whose chord_label deliberately prints nothing
        // there — a chord that fires but cannot be discovered or documented.
        // The mac-only intent is enforced, not just described.
        Mods::SuperShift => {
            (cfg!(target_os = "macos") && s.super_key() && s.shift_key()).then_some(Form::Super)
        }
    }
}

fn key_match(binding: &Binding, logical: &Key, physical: PhysicalKey, form: Form) -> bool {
    match (&binding.key, logical) {
        (ChordKey::Named(want), Key::Named(got)) => want == got,
        (ChordKey::Code(want), _) => physical == PhysicalKey::Code(*want),
        (ChordKey::Char(want), Key::Character(got)) => {
            // Case-folding is decided by the row *and* the spelling used.
            //
            // Under Ctrl+Shift the letter always arrives uppercase, because
            // Shift is spent on the modifier — `"T"`, never `"t"` — so folding
            // is the only way any row is reachable at all.
            //
            // Under ⌘, Shift is still free to shift, so ⌘⇧T is genuinely a
            // different chord from ⌘T and Desktop rows must stay exact or the
            // reserved reopen-closed-tab slot is burnt. The clipboard rows
            // fold under ⌘ too, and that is not an oversight: ⌘⇧C copied
            // before this table existed, and behaviour-preserving means it
            // still does.
            let fold = matches!(binding.mods, Mods::Clipboard) || form == Form::CtrlShift;
            if fold {
                got.eq_ignore_ascii_case(want)
            } else {
                got.as_str() == *want
            }
        }
        _ => false,
    }
}

/// First match in table order — table order is the dispatch precedence.
///
/// `physical` is the keycap's position, needed only by [`ChordKey::Code`] rows;
/// it is a third parameter rather than a `&KeyEvent` because winit's
/// `KeyEvent` carries a private platform field and cannot be constructed in a
/// test, which would take the whole of this module's test suite with it.
#[must_use]
pub fn lookup(
    logical: &Key,
    physical: PhysicalKey,
    mods: ModifiersState,
) -> Option<&'static Binding> {
    // Ctrl+Shift is the *only* way to reach a handful of control codes,
    // because the character they are named after is itself shifted: `@` is
    // NUL, `^` is RS, `_` is US. vim's `CTRL-^` — switch to the alternate
    // file — is that rule's most-used consequence, and a tab chord that ate
    // it would be a silent regression in the one editor most likely to be
    // running in here.
    //
    // Tested against the *logical* key, so it follows the keyboard rather than
    // a `cfg`: on a Swedish layout Shift+6 is `&`, which reaches no control
    // code, so Ctrl+Shift+6 switches tabs there and only a US-layout user
    // spends those two chords.
    if mods.control_key() && mods.shift_key() && !mods.super_key() {
        if let Key::Character(c) = logical {
            if matches!(c.as_str(), "@" | "^" | "_") {
                return None;
            }
        }
    }
    BINDINGS.iter().find(|binding| {
        mods_match(binding.mods, mods)
            .is_some_and(|form| key_match(binding, logical, physical, form))
    })
}

const MAC: bool = cfg!(target_os = "macos");

/// The platform-primary spelling of a chord.
///
/// Both chords work everywhere; the sheet shows the local convention and one
/// palette note says the other is accepted too — that fact lives in one place.
///
/// `Mods::Desktop` used to render as `Super+` off macOS, which was honest
/// about the code and useless to the user: Windows reserves Win+T, Win+K,
/// Win+P, Win+, and Win+1–9 for its own shell, so every one of those pills
/// named a chord that could not be pressed. Now that Desktop takes Ctrl+Shift
/// as well, the label says the reachable one.
#[must_use]
pub fn chord_label(binding: &Binding) -> String {
    let prefix = match binding.mods {
        Mods::Desktop => {
            if MAC {
                "⌘"
            } else {
                "Ctrl+Shift+"
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
        Mods::SuperShift => {
            if MAC {
                "⌘"
            } else {
                // No reachable spelling off macOS (see `Mods::SuperShift`):
                // an empty label draws no chip, which is honest, where
                // printing "Ctrl+Shift+…" would name the sibling's chord.
                return String::new();
            }
        }
    };
    // The keycap spells its own shift for the ⌘ forms — `⇧[`, `⇧E`. Every
    // non-mac prefix already ends in `Shift+`, so keeping it would print
    // `Ctrl+Shift+⇧E`.
    let keycap = if MAC { binding.keycap } else { binding.keycap.trim_start_matches('⇧') };
    format!("{prefix}{keycap}")
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
    MouseShortcut { gesture: "Click a block's rail", name: "Select that block" },
    MouseShortcut {
        gesture: "Right-click",
        name: "The selection, else a block's menu, else paste",
    },
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
            // The one place the both-conventions fact is displayed. It used to
            // be true of the clipboard rows alone, which is why it lives under
            // this group; it is now true of every chord in the table.
            rows.push(PaletteRow::Command {
                name: "Every shortcut takes ⌘ or Ctrl+Shift, on every platform".to_string(),
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

    /// A press whose position is irrelevant — every row but [`ChordKey::Code`].
    const NOWHERE: PhysicalKey = PhysicalKey::Code(KeyCode::F35);

    fn action_for(logical: &Key, mods: ModifiersState) -> Option<Action> {
        lookup(logical, NOWHERE, mods).map(|b| b.action)
    }

    /// A press the table must answer by *position*, so the character is free
    /// to be whatever that layout types — which is the whole point.
    fn action_at(logical: &Key, code: KeyCode, mods: ModifiersState) -> Option<Action> {
        lookup(logical, PhysicalKey::Code(code), mods).map(|b| b.action)
    }

    const SUPER: ModifiersState = ModifiersState::SUPER;
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const CTRL_SHIFT: ModifiersState = CTRL.union(SHIFT);

    #[test]
    fn the_table_resolves_every_chord_the_cascade_did() {
        // A pinned copy of the old if-cascade's behavior, including the
        // arrivals that are easy to get wrong. If a refactor of the table
        // changes any line of this, it changed a shortcut.
        let pinned: &[(Key, ModifiersState, Action)] = &[
            (char_key("t"), SUPER, Action::NewTab),
            (char_key("k"), SUPER, Action::ToggleFleetPicker),
            (char_key("w"), SUPER, Action::CloseTab),
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
        // The digits and the comma moved to positional matching, so they are
        // pinned by position. The cascade's behaviour is unchanged on any
        // layout whose number row types digits, which is the only kind the
        // cascade ever worked on.
        let positional: &[(KeyCode, Action)] = &[
            (KeyCode::Digit1, Action::ActivateTab(0)),
            (KeyCode::Digit8, Action::ActivateTab(7)),
            (KeyCode::Digit9, Action::ActivateLastTab),
            (KeyCode::Comma, Action::ToggleSettings),
        ];
        for (code, want) in positional {
            assert_eq!(
                action_at(&char_key("unused"), *code, SUPER),
                Some(*want),
                "{code:?} + ⌘ must resolve to {want:?} whatever that key types"
            );
        }
    }

    #[test]
    fn every_desktop_chord_has_a_ctrl_shift_form() {
        // The policy, pinned. It fails the day someone adds a Desktop row
        // Windows cannot reach -- which is the state this whole family was in
        // until now, because Win+T, Win+K, Win+P, Win+, and Win+1-9 all belong
        // to the Windows shell and never arrive.
        for binding in BINDINGS.iter().filter(|b| b.mods == Mods::Desktop) {
            let got = match binding.key {
                // Shift is spent on the modifier, so the letter arrives
                // uppercase.
                ChordKey::Char(c) => action_for(&char_key(&c.to_ascii_uppercase()), CTRL_SHIFT),
                // Deliberately a *wrong* character: a positional row must not
                // care what the key types, and passing the right one would
                // prove nothing.
                ChordKey::Code(code) => action_at(&char_key("\u{0}"), code, CTRL_SHIFT),
                ChordKey::Named(n) => action_for(&Key::Named(n), CTRL_SHIFT),
            };
            assert_eq!(
                got,
                Some(binding.action),
                "'{}' is reachable with ⌘ and must be reachable with Ctrl+Shift",
                binding.name
            );
        }
    }

    #[test]
    fn the_super_form_stays_exact_so_the_reserved_chord_survives() {
        assert_eq!(
            action_for(&char_key("T"), SUPER.union(SHIFT)),
            None,
            "⌘⇧T is held for reopen-closed-tab; folding it onto ⌘T would burn the slot"
        );
        assert_eq!(
            action_for(&char_key("T"), CTRL_SHIFT),
            Some(Action::NewTab),
            "…while Ctrl+Shift+T has no unshifted spelling at all, so it must fold"
        );
    }

    #[test]
    fn unbound_ctrl_shift_chords_still_reach_the_shell() {
        // The regression the whole design exists to avoid. `key::encode` runs
        // only when the table declines, so a table that over-matches silently
        // swallows real terminal input.
        for (logical, what) in [
            (char_key("A"), "Ctrl+Shift+A is nothing of ours"),
            (Key::Named(NamedKey::ArrowLeft), "vim and tmux read Ctrl+Shift+Arrow"),
            (Key::Named(NamedKey::F5), "full-screen apps read the Ctrl+Shift F-keys"),
            (Key::Named(NamedKey::Home), "editors read Ctrl+Shift+Home"),
        ] {
            assert_eq!(action_for(&logical, CTRL_SHIFT), None, "{what}");
        }
    }

    #[test]
    fn the_control_codes_only_shift_can_reach_are_not_stolen() {
        // On a US layout these three characters are *only* reachable with
        // Shift held, and each encodes a control byte: @ is NUL, ^ is RS,
        // _ is US. vim's CTRL-^ -- switch to the alternate file -- is the
        // most-used of them, and a tab chord eating it would be invisible.
        assert_eq!(
            action_at(&char_key("^"), KeyCode::Digit6, CTRL_SHIFT),
            None,
            "Ctrl+Shift+6 is vim's CTRL-^ on a US layout, not 'go to tab 6'"
        );
        assert_eq!(
            action_at(&char_key("@"), KeyCode::Digit2, CTRL_SHIFT),
            None,
            "Ctrl+Shift+2 is the only way to send NUL"
        );
        // …and the guard follows the keyboard rather than a `cfg`: on a
        // Swedish layout Shift+6 is `&`, which encodes nothing, so the chord
        // is free and the tab switch stands.
        assert_eq!(
            action_at(&char_key("&"), KeyCode::Digit6, CTRL_SHIFT),
            Some(Action::ActivateTab(5)),
            "a layout where the shifted digit reaches no control code keeps its tab chord"
        );
    }

    #[test]
    fn the_rows_ctrl_shift_already_owned_are_not_shadowed() {
        assert_eq!(
            action_for(&Key::Named(NamedKey::Tab), CTRL_SHIFT),
            Some(Action::PrevTab),
            "Ctrl+Shift+Tab cycled backwards before the Desktop family widened"
        );
        assert_eq!(
            action_for(&Key::Named(NamedKey::PageUp), CTRL_SHIFT),
            Some(Action::ScrollPageUp),
            "Ctrl+Shift+PgUp paged before it too"
        );
        assert_eq!(
            action_for(&char_key("C"), CTRL_SHIFT),
            Some(Action::Copy),
            "and the clipboard rows are untouched"
        );
    }

    #[test]
    fn reserved_and_bare_keys_never_match() {
        assert_eq!(
            action_for(&char_key("T"), SUPER.union(SHIFT)),
            None,
            "⌘⇧T is reserved for reopen-closed-tab; matching it as ⌘T would burn the slot"
        );
        for binding in BINDINGS {
            match binding.key {
                ChordKey::Char(c) => assert_eq!(
                    action_for(&char_key(c), ModifiersState::empty()),
                    None,
                    "bare '{c}' must reach the shell, not the app — otherwise typing it acts"
                ),
                ChordKey::Code(code) => assert_eq!(
                    action_at(&char_key("1"), code, ModifiersState::empty()),
                    None,
                    "bare {code:?} must reach the shell — a positional row is still a chord"
                ),
                ChordKey::Named(_) => {}
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
    fn shift_comma_opens_profiles_and_plain_comma_stays_settings() {
        // Design §12's chord pair: ⌘, is Settings, ⌘⇧, is Profiles. The
        // profiles row must sit before the settings row (Desktop ignores
        // Shift under Super) or ⌘⇧, silently stays Settings — the state it
        // was in before this binding existed.
        if MAC {
            assert_eq!(
                action_at(&char_key("<"), KeyCode::Comma, SUPER.union(SHIFT)),
                Some(Action::OpenProfiles),
                "⌘⇧, opens the Profiles tab"
            );
        } else {
            // Off macOS `super` is the Win key, and the SuperShift row is
            // deliberately inert: Win+Shift+, firing an action whose label
            // prints nothing would be a chord that runs but cannot be
            // discovered. It falls through to Desktop's shift-blind comma —
            // the Settings meaning the unshifted chord already has.
            assert_eq!(
                action_at(&char_key("<"), KeyCode::Comma, SUPER.union(SHIFT)),
                Some(Action::ToggleSettings),
                "Win+Shift+, must not fire the undiscoverable Profiles chord"
            );
        }
        assert_eq!(
            action_at(&char_key(","), KeyCode::Comma, SUPER),
            Some(Action::ToggleSettings),
            "⌘, without shift stays Settings"
        );
        // On Windows both mac spellings collapse onto Ctrl+Shift+, — which
        // keeps its established meaning. Profiles is reachable there via
        // the palette row and the launcher's Manage-profiles row instead.
        assert_eq!(
            action_at(&char_key("<"), KeyCode::Comma, CTRL_SHIFT),
            Some(Action::ToggleSettings),
            "Ctrl+Shift+, keeps Settings; the shifted sibling must not steal it"
        );
        if !MAC {
            let profiles = BINDINGS
                .iter()
                .find(|b| b.action == Action::OpenProfiles)
                .expect("the profiles binding exists");
            assert_eq!(
                chord_label(profiles),
                "",
                "off macOS the label is empty — a chip naming Ctrl+Shift+, would lie"
            );
        }
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

    #[test]
    fn chord_labels_never_double_the_shift() {
        // Off macOS the prefix already ends in `Shift+`, so a keycap that
        // spells its own `⇧` would print `Ctrl+Shift+⇧E`. On macOS `⌘⇧E` is
        // exactly right. One test, both legs of CI asserting something.
        for binding in BINDINGS.iter().filter(|b| b.mods == Mods::Desktop) {
            let label = chord_label(binding);
            if MAC {
                assert!(
                    label.starts_with('⌘'),
                    "the Mac spelling of '{}' is ⌘-prefixed: {label}",
                    binding.name
                );
            } else {
                assert!(
                    label.starts_with("Ctrl+Shift+") && !label.contains('⇧'),
                    "'{}' must name the chord Windows can actually deliver: {label}",
                    binding.name
                );
            }
        }
    }
}
