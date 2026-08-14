//! The app's one text-input primitive: a string, a caret and a selection.
//!
//! Pure on purpose, like `chrome::layout` and `settings_ui`: no window, no
//! GPU, no clipboard handle. Paste text arrives as an argument and a copy
//! leaves as a return value, so every rule about what a key does to a buffer
//! is testable without opening anything.
//!
//! Before this existed each entry — the settings edit buffer, the profiles
//! edit buffer, four filter boxes, the enrolment code — hand-rolled its own
//! `Key::Character` arm over a bare `String`, and six of the seven swallowed
//! ⌘V (#251). The guard that makes a chord "not text" is `super_key()`, which
//! is also the paste chord; the `return` underneath it is what stopped the
//! global keymap table from ever seeing the key. One primitive, consulted
//! first, is how that stops being a thing each new entry can forget.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// An editable string with a caret and a selection.
///
/// `caret` and `anchor` are **byte** indices, always on a `char` boundary;
/// `anchor == caret` means there is no selection. Byte indices rather than
/// character counts because every consumer — slicing for the renderer's
/// measurement, `String::insert_str`, `drain` — wants bytes, and converting at
/// each of those is where an off-by-one would live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    text: String,
    caret: usize,
    anchor: usize,
}

/// What one command did.
///
/// `copied` is returned rather than written because this module never touches
/// `arboard`. `changed` separates an edit from a bare caret move, which the
/// filter boxes need: typing resets the selection to the first match, and ←
/// must not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextEdit {
    pub copied: Option<String>,
    pub changed: bool,
}

/// What a key means to a text field.
///
/// `command_for` returns `None` for anything that is *not* text — Enter,
/// Escape, Tab, an unmodified ↑/↓ — so the caller falls through to its own
/// navigation handling. That is the whole contract: a site consults this
/// first, and whatever it declines is still the site's to interpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextCommand {
    Insert(String),
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteWordForward,
    /// ⌘⌫ — everything behind the caret.
    DeleteToStart,
    Move { right: bool, word: bool, extend: bool },
    Home { extend: bool },
    End { extend: bool },
    SelectAll,
    Copy,
    Cut,
    Paste,
}

impl TextField {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self { text, caret: end, anchor: end }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Byte index of the caret — what the renderer measures a prefix up to.
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected byte range, low end first, or `None` when nothing is
    /// selected.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        (self.anchor != self.caret)
            .then(|| (self.anchor.min(self.caret), self.anchor.max(self.caret)))
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(a, b)| &self.text[a..b])
    }

    /// Replace the whole contents, caret at the end — seeding an edit.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// Everything selected, caret at the end.
    ///
    /// Deliberately *not* what opening an edit does: a settings edit seeds
    /// with the current value and puts the caret after it, so Enter-then-type
    /// still appends the way it always has. ⌘A is how you replace.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    /// Insert at the caret, replacing the selection if there is one.
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = self.caret;
    }

    /// Remove and return the selection — the cut half of ⌘X.
    pub fn take_selection(&mut self) -> Option<String> {
        let taken = self.selected_text()?.to_string();
        self.delete_selection();
        Some(taken)
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(prev) = self.prev_boundary(self.caret) {
            self.text.drain(prev..self.caret);
            self.caret = prev;
            self.anchor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(next) = self.next_boundary(self.caret) {
            self.text.drain(self.caret..next);
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.word_boundary(false);
        self.text.drain(to..self.caret);
        self.caret = to;
        self.anchor = to;
    }

    pub fn delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.word_boundary(true);
        self.text.drain(self.caret..to);
    }

    pub fn delete_to_start(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.text.drain(..self.caret);
        self.caret = 0;
        self.anchor = 0;
    }

    /// Move the caret. `extend` keeps the anchor (shift-selecting); without
    /// it, moving *off* a selection collapses to the edge you moved towards
    /// rather than to where the caret happened to be — the behaviour every
    /// text field has and the one people notice when it is missing.
    pub fn move_caret(&mut self, right: bool, word: bool, extend: bool) {
        if !extend {
            if let Some((lo, hi)) = self.selection() {
                self.caret = if right { hi } else { lo };
                self.anchor = self.caret;
                if !word {
                    return;
                }
            }
        }
        self.caret = if word {
            self.word_boundary(right)
        } else if right {
            self.next_boundary(self.caret).unwrap_or(self.caret)
        } else {
            self.prev_boundary(self.caret).unwrap_or(self.caret)
        };
        if !extend {
            self.anchor = self.caret;
        }
    }

    pub fn home(&mut self, extend: bool) {
        self.caret = 0;
        if !extend {
            self.anchor = 0;
        }
    }

    pub fn end(&mut self, extend: bool) {
        self.caret = self.text.len();
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Run a command. `clipboard` is the text a [`TextCommand::Paste`] should
    /// insert — the caller reads it, because this module has no handle.
    ///
    /// `changed` is measured against the text rather than inferred from the
    /// command, so a paste with an empty clipboard and a ⌘C over nothing both
    /// report honestly.
    pub fn apply(&mut self, cmd: TextCommand, clipboard: Option<&str>) -> TextEdit {
        // Compared by value, not by length: replacing a selection with text
        // the same size is still an edit.
        let before = (!matches!(
            cmd,
            TextCommand::Copy
                | TextCommand::Move { .. }
                | TextCommand::Home { .. }
                | TextCommand::End { .. }
                | TextCommand::SelectAll
        ))
        .then(|| self.text.clone());
        let copied = match cmd {
            TextCommand::Insert(s) => {
                self.insert(&s);
                None
            }
            TextCommand::Backspace => {
                self.backspace();
                None
            }
            TextCommand::Delete => {
                self.delete();
                None
            }
            TextCommand::DeleteWordBack => {
                self.delete_word_back();
                None
            }
            TextCommand::DeleteWordForward => {
                self.delete_word_forward();
                None
            }
            TextCommand::DeleteToStart => {
                self.delete_to_start();
                None
            }
            TextCommand::Move { right, word, extend } => {
                self.move_caret(right, word, extend);
                None
            }
            TextCommand::Home { extend } => {
                self.home(extend);
                None
            }
            TextCommand::End { extend } => {
                self.end(extend);
                None
            }
            TextCommand::SelectAll => {
                self.select_all();
                None
            }
            TextCommand::Copy => self.selected_text().map(str::to_string),
            TextCommand::Cut => self.take_selection(),
            // An empty clipboard is a no-op, not an empty insert: pasting
            // nothing must not silently delete the selection.
            TextCommand::Paste => {
                if let Some(text) = clipboard.filter(|t| !t.is_empty()) {
                    self.insert(text);
                }
                None
            }
        };
        TextEdit { copied, changed: before.is_some_and(|b| b != self.text) }
    }

    /// True when there was a selection to remove.
    fn delete_selection(&mut self) -> bool {
        let Some((lo, hi)) = self.selection() else { return false };
        self.text.drain(lo..hi);
        self.caret = lo;
        self.anchor = lo;
        true
    }

    /// Step by `char`, not by grapheme cluster.
    ///
    /// Deliberate, and not the half-fix it looks like: a cluster-aware caret
    /// wants `unicode-segmentation` as a direct dependency and a decision
    /// about what a *selection* across a ZWJ sequence means, which this
    /// change does not need. Stepping by `char` is at least always on a valid
    /// boundary — `text.len()` arithmetic is what panics, and is what the
    /// browser client already paid for by counting UTF-16 units.
    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.text[at..].chars().next().map(|c| at + c.len_utf8())
    }

    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.text[..at].chars().next_back().map(|c| at - c.len_utf8())
    }

    /// The next word edge in `right`'s direction: skip the run of separators
    /// under the caret, then the run of word characters. `/`, `-`, `.` and `_`
    /// count as separators — the fields holding a path or a font family are
    /// the ones ⌥← exists for.
    fn word_boundary(&self, right: bool) -> usize {
        let is_sep = |c: char| !c.is_alphanumeric();
        let mut at = self.caret;
        if right {
            while let Some(c) = self.text[at..].chars().next() {
                if !is_sep(c) {
                    break;
                }
                at += c.len_utf8();
            }
            while let Some(c) = self.text[at..].chars().next() {
                if is_sep(c) {
                    break;
                }
                at += c.len_utf8();
            }
        } else {
            while let Some(c) = self.text[..at].chars().next_back() {
                if !is_sep(c) {
                    break;
                }
                at -= c.len_utf8();
            }
            while let Some(c) = self.text[..at].chars().next_back() {
                if is_sep(c) {
                    break;
                }
                at -= c.len_utf8();
            }
        }
        at
    }
}

/// What this key means to a text field, or `None` if it is not text.
///
/// Clipboard chords go through [`zest_input::key::is_clipboard_chord`], so ⌘C
/// and Ctrl+Shift+C both work in a field exactly as they do in the grid — a
/// field that took only one of them is a field that appears broken on the
/// other platform.
#[must_use]
pub fn command_for(key: &Key, mods: ModifiersState) -> Option<TextCommand> {
    use zest_input::key;

    let shift = mods.shift_key();
    // Word-wise motion is ⌥ on macOS and Ctrl elsewhere. Accepting both
    // everywhere costs nothing: neither chord means anything else in a field.
    let word = mods.alt_key() || mods.control_key();

    match key {
        Key::Named(named) => Some(match named {
            // ⌘⌫ is "delete to the start of the line" on macOS. Checked
            // before the word-wise arm because ⌥⌫ and ⌘⌫ are different verbs
            // and a chord carrying both should mean the wider one.
            NamedKey::Backspace if mods.super_key() => TextCommand::DeleteToStart,
            NamedKey::Backspace if word => TextCommand::DeleteWordBack,
            NamedKey::Backspace => TextCommand::Backspace,
            NamedKey::Delete if word => TextCommand::DeleteWordForward,
            NamedKey::Delete => TextCommand::Delete,
            // ⌘←/→ is line-wise on macOS, where a word-wise ⌥ already exists.
            NamedKey::ArrowLeft if mods.super_key() => TextCommand::Home { extend: shift },
            NamedKey::ArrowRight if mods.super_key() => TextCommand::End { extend: shift },
            NamedKey::ArrowLeft => TextCommand::Move { right: false, word, extend: shift },
            NamedKey::ArrowRight => TextCommand::Move { right: true, word, extend: shift },
            NamedKey::Home => TextCommand::Home { extend: shift },
            NamedKey::End => TextCommand::End { extend: shift },
            // Space arrives named, not as a character, and a path or a font
            // family is full of them.
            NamedKey::Space if !mods.super_key() && !mods.control_key() => {
                TextCommand::Insert(" ".to_string())
            }
            _ => return None,
        }),
        Key::Character(c) => {
            if key::is_clipboard_chord(mods) {
                return match c.as_str() {
                    "c" | "C" => Some(TextCommand::Copy),
                    "v" | "V" => Some(TextCommand::Paste),
                    "x" | "X" => Some(TextCommand::Cut),
                    // ⌘A is select-all only under Super: Ctrl+Shift+A is not a
                    // convention anywhere, and Ctrl+A is start-of-line in the
                    // shell, which the grid must keep.
                    "a" | "A" if mods.super_key() => Some(TextCommand::SelectAll),
                    _ => None,
                };
            }
            // Anything else carrying a desktop modifier is a chord, not text —
            // ⌘X must not put an `x` in the buffer. Alt is the exception on
            // macOS, where ⌥ composes (⌥5 is €).
            if mods.super_key() || mods.control_key() {
                return None;
            }
            Some(TextCommand::Insert(c.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPER: ModifiersState = ModifiersState::SUPER;
    const CTRL_SHIFT: ModifiersState =
        ModifiersState::CONTROL.union(ModifiersState::SHIFT);

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn paste_replaces_the_selection() {
        // The bug this primitive exists for: ⌘A ⌘V must replace, not append.
        let mut f = TextField::new("/bin/zsh");
        f.select_all();
        f.apply(TextCommand::Paste, Some("/usr/local/bin/fish"));
        assert_eq!(f.text(), "/usr/local/bin/fish", "the selection was replaced");
        assert_eq!(f.caret(), f.text().len(), "and the caret followed the insert");
        assert_eq!(f.selection(), None, "an insert collapses the selection");
    }

    #[test]
    fn both_clipboard_chords_paste() {
        // Ctrl+Shift+V is the Windows/Linux spelling and ⌘V the macOS one; a
        // field that takes only one appears broken on the other platform.
        for mods in [SUPER, CTRL_SHIFT] {
            assert_eq!(
                command_for(&ch("v"), mods),
                Some(TextCommand::Paste),
                "{mods:?} carries the paste chord"
            );
        }
    }

    #[test]
    fn a_paste_chord_never_types_its_own_letter() {
        // The value picker's filter had no modifier guard at all, so ⌘V typed
        // a literal `v` into it (#251). Every clipboard letter must decode to
        // its command and never to text.
        for letter in ["c", "v", "x"] {
            let cmd = command_for(&ch(letter), SUPER);
            assert!(
                !matches!(cmd, Some(TextCommand::Insert(_))),
                "⌘{letter} is a chord, not text — got {cmd:?}"
            );
        }
        assert_eq!(
            command_for(&ch("v"), ModifiersState::empty()),
            Some(TextCommand::Insert("v".to_string())),
            "an unmodified v is still a letter"
        );
    }

    #[test]
    fn a_desktop_chord_is_never_text() {
        // ⌘W closes a tab; it must not leave a `w` behind on the way out.
        assert_eq!(command_for(&ch("w"), SUPER), None, "⌘W is not text");
        assert_eq!(
            command_for(&ch("t"), ModifiersState::CONTROL),
            None,
            "Ctrl+T is not text"
        );
    }

    #[test]
    fn copy_and_cut_hand_back_the_selection() {
        let mut f = TextField::new("obsidian");
        f.select_all();
        let copy = f.apply(TextCommand::Copy, None);
        assert_eq!(copy.copied.as_deref(), Some("obsidian"), "copy hands back the selection");
        assert!(!copy.changed, "and reports the text untouched");
        assert_eq!(f.text(), "obsidian", "because it is");

        let cut = f.apply(TextCommand::Cut, None);
        assert_eq!(cut.copied.as_deref(), Some("obsidian"), "cut hands back the same text");
        assert!(cut.changed, "and reports the edit");
        assert_eq!(f.text(), "", "having removed it");
    }

    #[test]
    fn copy_with_no_selection_leaves_the_clipboard_alone() {
        // Otherwise a stray ⌘C in a field silently replaces whatever the user
        // had copied from the grid.
        let mut f = TextField::new("nord");
        assert_eq!(
            f.apply(TextCommand::Copy, None),
            TextEdit::default(),
            "nothing selected, nothing copied"
        );
    }

    #[test]
    fn an_empty_clipboard_does_not_eat_the_selection() {
        let mut f = TextField::new("nord");
        f.select_all();
        let edit = f.apply(TextCommand::Paste, None);
        assert_eq!(f.text(), "nord", "a paste with nothing to paste changes nothing");
        assert!(!edit.changed, "and says so, so a filter's selection does not jump");
    }

    #[test]
    fn the_caret_crosses_multibyte_text_without_panicking() {
        // Byte indices and `char` stepping have to agree, or every one of
        // these is a slice on a non-boundary and a panic in a settings field.
        let mut f = TextField::new("héllo→🌍");
        for _ in 0..20 {
            f.move_caret(false, false, false);
        }
        assert_eq!(f.caret(), 0, "walking left off the end stops at zero");
        for _ in 0..20 {
            f.move_caret(true, false, false);
        }
        assert_eq!(f.caret(), f.text().len(), "and right stops at the end");
        f.backspace();
        assert_eq!(f.text(), "héllo→", "backspace removes one whole char, not one byte");
    }

    #[test]
    fn word_motion_stops_at_path_separators() {
        // ⌥← in a `shell.command` field is the reason word motion exists.
        let mut f = TextField::new("/usr/local/bin/fish");
        f.move_caret(false, true, false);
        assert_eq!(&f.text()[f.caret()..], "fish", "one word back is the last segment");
        f.move_caret(false, true, false);
        assert_eq!(&f.text()[f.caret()..], "bin/fish", "the separator is skipped, not landed on");
    }

    #[test]
    fn word_delete_removes_one_segment() {
        let mut f = TextField::new("/usr/local/bin");
        f.apply(TextCommand::DeleteWordBack, None);
        assert_eq!(f.text(), "/usr/local/", "the trailing segment went, the separator stayed");
    }

    #[test]
    fn collapsing_a_selection_lands_on_the_edge_you_moved_towards() {
        let mut f = TextField::new("obsidian");
        f.select_all();
        f.move_caret(false, false, false);
        assert_eq!(f.caret(), 0, "← off a selection goes to its start");
        f.select_all();
        f.move_caret(true, false, false);
        assert_eq!(f.caret(), 8, "→ off a selection goes to its end");
    }

    #[test]
    fn shift_extends_and_home_end_reach_the_edges() {
        let mut f = TextField::new("paper");
        f.home(false);
        f.end(true);
        assert_eq!(f.selected_text(), Some("paper"), "shift-End selected to the end");
        assert_eq!(
            command_for(&Key::Named(NamedKey::Home), ModifiersState::SHIFT),
            Some(TextCommand::Home { extend: true }),
            "shift-Home extends"
        );
    }

    #[test]
    fn enter_and_escape_are_not_text() {
        // The contract that lets every call site keep its own navigation: a
        // key this declines is still the site's to interpret.
        for named in [NamedKey::Enter, NamedKey::Escape, NamedKey::Tab, NamedKey::ArrowUp] {
            assert_eq!(
                command_for(&Key::Named(named), ModifiersState::empty()),
                None,
                "{named:?} belongs to the caller, not to the field"
            );
        }
    }

    #[test]
    fn space_arrives_named_and_is_still_text() {
        assert_eq!(
            command_for(&Key::Named(NamedKey::Space), ModifiersState::empty()),
            Some(TextCommand::Insert(" ".to_string())),
            "a font family with a space in it must be typeable"
        );
    }
}
