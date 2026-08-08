//! Input-method composition: dead keys, and every script that needs more keys
//! than a keyboard has.
//!
//! Two things arrive from the platform. **Preedit** is provisional text the user
//! is still composing — `´` after `Option+e`, or the kana of a half-typed
//! Japanese word — and **commit** is the finished text. Only the commit is real.
//!
//! # The preedit is not terminal content
//!
//! It never enters the grid and never crosses the wire. A composing buffer is a
//! fact about the keyboard in front of *this* person, and the grid is shared:
//! with the daemon, with the renderer's scrollback, with anyone else attached to
//! the same session. Writing provisional text into it would put half-typed
//! characters into another device's scrollback and into whatever the shell reads
//! back. The daemon receives exactly what the pty would have received from a
//! physical keyboard — the commit, once — which is the same rule the rest of
//! this crate follows: encoding happens at the keyboard.
//!
//! So the preedit lives here, the app draws it over the cursor, and the shell
//! never learns it existed.
//!
//! # Why enabling this is not free
//!
//! `Window::set_ime_allowed(true)` is what delivers these events at all, and on
//! macOS it is also what makes dead-key sequences combine — without it
//! `Option+e` `e` does not produce `é`. It changes ordinary typing too: winit
//! stops sending `KeyboardInput` for the duration of a preedit, which is what
//! keeps the raw keystrokes of a composition out of the pty. [`Ime::composing`]
//! guards the same thing on this side, because "the platform will not send it"
//! is a promise made by four backends rather than a property of the code.

/// Text being composed, and where the input method wants its own caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// Byte range within `text` the input method has highlighted. `None` means
    /// its caret is hidden.
    ///
    /// Byte-wise, not char-wise, exactly as winit reports it — converting here
    /// would be one more place to get a multi-byte boundary wrong.
    pub cursor: Option<(usize, usize)>,
}

/// Composition state for one window.
#[derive(Debug, Default)]
pub struct Ime {
    /// Whether the platform has an input method active on this window.
    ///
    /// Tracked but deliberately not used as a gate: a composition is in progress
    /// when there is preedit text, and nothing else. On some backends `Enabled`
    /// arrives once at focus and stays true for the life of the window, so
    /// gating on it would swallow every keystroke forever.
    enabled: bool,
    preedit: Option<Preedit>,
}

impl Ime {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// The input method went away. Anything half-composed goes with it.
    ///
    /// Dropping rather than committing it: the platform did not say the text was
    /// finished, and inserting an abandoned composition into a shell prompt is a
    /// command the user never typed.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.preedit = None;
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Replace the composing text. An empty string clears it.
    ///
    /// winit sends an empty preedit immediately before every commit, so this is
    /// also the normal path out of a composition.
    pub fn set_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) {
        self.preedit = if text.is_empty() { None } else { Some(Preedit { text, cursor }) };
    }

    /// The composition finished. The caller sends the text; this just forgets.
    pub fn commit(&mut self) {
        self.preedit = None;
    }

    #[must_use]
    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }

    /// Whether a composition is in progress.
    ///
    /// While this is true, key events must not be encoded and sent. The keys
    /// belong to the input method: `Enter` confirms a candidate rather than
    /// running a command, and the arrows move through the candidate list.
    #[must_use]
    pub fn composing(&self) -> bool {
        self.preedit.is_some()
    }

    /// How many cells the composing text occupies, for drawing and for the
    /// candidate window's placement.
    ///
    /// Zero when nothing is composing.
    #[must_use]
    pub fn width_cells(&self) -> usize {
        self.preedit.as_ref().map_or(0, |p| preedit_width(&p.text))
    }
}

/// Display width of composing text, in terminal cells.
///
/// The same rule the grid uses, so a preedit occupies exactly the cells the
/// committed text will: CJK and emoji take two, combining marks take none.
#[must_use]
pub fn preedit_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_preedit_ends_the_composition() {
        // winit sends this immediately before every commit. Treating it as a
        // one-character-wide composition would leave a stray highlighted cell
        // over the prompt after every word typed with an input method.
        let mut ime = Ime::new();
        ime.set_preedit("か".into(), None);
        assert!(ime.composing());
        ime.set_preedit(String::new(), None);
        assert!(!ime.composing(), "an empty preedit is a cleared preedit");
        assert!(ime.preedit().is_none());
    }

    #[test]
    fn composing_swallows_keys_and_finishing_stops() {
        // The property the whole module exists for. While composing, `Enter`
        // confirms a candidate; if the terminal also encoded it, choosing a
        // Japanese word would run whatever was on the prompt.
        let mut ime = Ime::new();
        assert!(!ime.composing(), "nothing composed means keys are the terminal's");
        ime.set_preedit("にほ".into(), Some((6, 6)));
        assert!(ime.composing());
        ime.commit();
        assert!(!ime.composing(), "after a commit the keyboard is the terminal's again");
    }

    #[test]
    fn a_disabled_input_method_drops_what_was_half_typed() {
        // Committing it instead would insert a command the user never finished
        // typing -- the worst possible failure at a shell prompt.
        let mut ime = Ime::new();
        ime.enable();
        ime.set_preedit("にほんご".into(), None);
        ime.disable();
        assert!(!ime.composing());
        assert!(!ime.enabled());
    }

    #[test]
    fn enabled_alone_never_swallows_keys() {
        // Several backends send `Enabled` once when the window takes focus and
        // never send `Disabled`. Gating on it rather than on preedit text would
        // make the terminal ignore the keyboard for the life of the window.
        let mut ime = Ime::new();
        ime.enable();
        assert!(!ime.composing(), "enabled is not composing");
    }

    #[test]
    fn preedit_width_matches_what_the_grid_will_use() {
        assert_eq!(preedit_width("abc"), 3);
        assert_eq!(preedit_width("にほんご"), 8, "CJK takes two cells each");
        assert_eq!(preedit_width("é"), 1);
        // A combining mark occupies no cell of its own, exactly as in the grid.
        assert_eq!(preedit_width("e\u{0301}"), 1);
        assert_eq!(preedit_width(""), 0);
    }

    #[test]
    fn the_cursor_range_survives_round_trip() {
        // Byte indices, as winit reports them. Silently re-basing them to chars
        // is how a candidate highlight lands mid-codepoint.
        let mut ime = Ime::new();
        ime.set_preedit("にほんご".into(), Some((3, 9)));
        assert_eq!(ime.preedit().unwrap().cursor, Some((3, 9)));
    }
}
