//! Named keys, encoded here rather than spelled by the caller.
//!
//! An agent has no keyboard. Every other client of this protocol encodes a
//! keystroke at the keyboard that produced it -- `zest-input` from a
//! `winit::KeyEvent`, `clients/web/packages/input` from a `KeyboardEvent` --
//! because modifier conventions belong to the platform that produced them.
//! There is no such platform here, so the encoding happens on this side of the
//! wire and the caller names a key instead.
//!
//! That is not only a convenience. Arrows are mode-dependent: DECCKM (`CSI ? 1
//! h`) switches the whole cursor family from `ESC [ A` to `ESC O A`, and the
//! mode lives on the *host*. A model writing an escape sequence into a JSON
//! string is therefore right about half the time and silently does nothing the
//! rest -- measured at roughly 2 attempts in 10 reaching the application, with
//! the remainder arriving as literal text (#345).
//!
//! # This is the third copy of one table, and what keeps it honest
//!
//! `crates/zest-input/src/key.rs` is the source of truth;
//! `clients/web/packages/input/src/key.ts` is a case-for-case port of it, and
//! says so. This is the third, and it exists because `zest-input` takes `winit`
//! types in its public API while this is a headless stdio binary.
//!
//! Three copies of one rule is precisely the shape that gave
//! `Grid::drop_scrollback_rows` three different semantics. So this one is not
//! held by review: `tests/keys.rs` sweeps every name in the table against
//! `zest_input::key::encode_press` -- all 27 bases, every ctrl/alt/shift
//! combination, DECCKM on and off -- and byte-inequality fails the build. Copy
//! the reference verbatim, quirks included; the sweep will tell you if you
//! improved something.
//!
//! # The kitty protocol is deliberately not here
//!
//! `zest_input::key::encode` branches to a CSI-u encoder whenever a program has
//! pushed kitty flags. Porting that would be a second large table and the same
//! trap again. This encodes the legacy forms unconditionally, which every
//! program accepting the kitty protocol also accepts -- that is the protocol's
//! own compatibility rule. `the_table_ignores_kitty_flags` asserts it rather
//! than leaving it to be inferred from an absence.

use std::str::FromStr;

use zest_core::Modes;

/// The terminal modifier set.
///
/// Ctrl, Alt and Shift, and no more: there is no escape sequence for
/// Super/Command and no application expects one, so a chord carrying it belongs
/// to a window manager rather than to a shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// The xterm modifier parameter: 1 + bitmask.
    fn param(self) -> u8 {
        let mut m = 0;
        if self.shift {
            m |= 1;
        }
        if self.alt {
            m |= 2;
        }
        if self.ctrl {
            m |= 4;
        }
        m + 1
    }
}

/// A key with a name rather than a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Insert,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// `F1`-`F12`. Nothing above 12: the reference encoder returns nothing for
    /// them, so naming one here would be a key that parses and does nothing.
    F(u8),
}

/// Every base this table accepts, in the order the tool description lists them.
pub const NAMES: &[(&str, Named)] = &[
    ("enter", Named::Enter),
    ("esc", Named::Esc),
    ("tab", Named::Tab),
    ("backspace", Named::Backspace),
    ("delete", Named::Delete),
    ("insert", Named::Insert),
    ("space", Named::Space),
    ("up", Named::Up),
    ("down", Named::Down),
    ("left", Named::Left),
    ("right", Named::Right),
    ("home", Named::Home),
    ("end", Named::End),
    ("pageup", Named::PageUp),
    ("pagedown", Named::PageDown),
    // Aliases. Spelled out rather than fuzzy-matched: a model that guesses
    // `escape` should succeed, and one that guesses `escpae` should be told.
    ("escape", Named::Esc),
    ("return", Named::Enter),
    ("del", Named::Delete),
    ("ins", Named::Insert),
    ("pgup", Named::PageUp),
    ("pgdn", Named::PageDown),
    ("pgdown", Named::PageDown),
];

/// What a chord is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Named(Named),
    Char(char),
}

/// A parsed key: a base, and the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub base: Base,
    pub mods: Mods,
}

/// Why a key name was refused.
///
/// Refused, never ignored. A key that silently does nothing is exactly the
/// failure #345 reports -- "bytes arriving, redraw happening, no key" -- and
/// from inside an agent loop it is indistinguishable from a key the application
/// chose not to handle. Every arm names what to do instead, in the style
/// [`crate::run::Refusal`] established.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("an empty key name. Use one of: {vocab}", vocab = VOCABULARY)]
    Empty,
    #[error(
        "`{name}` is not a key name. Use one of: {vocab}. Type ordinary characters with `text` \
         instead",
        vocab = VOCABULARY
    )]
    Unknown { name: String },
    #[error(
        "`{0}`: there is no terminal encoding for Command/Super and no application expects one \
         -- the terminal modifiers are ctrl, alt and shift"
    )]
    Desktop(String),
    #[error("`{0}` is a character, not a key -- use `text` to type it")]
    BareCharacter(String),
    #[error(
        "`{0}`: use `text` to type capitals. `shift` here modifies a named key, and this table \
         cannot invent the character a shifted letter produces on someone else's keyboard layout"
    )]
    ShiftedCharacter(String),
}

/// The vocabulary, as one sentence a model can act on.
pub const VOCABULARY: &str = "enter, esc, tab, backspace, delete, insert, space, up, down, \
     left, right, home, end, pageup, pagedown, f1-f12, optionally prefixed with ctrl+, alt+ \
     and/or shift+; or ctrl+<letter> for a control code";

impl FromStr for Chord {
    type Err = KeyError;

    /// `[modifier+]* base`, case-insensitive, `_ - ` ignored inside a base name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let spelled = s.trim();
        if spelled.is_empty() {
            return Err(KeyError::Empty);
        }
        let lowered = spelled.to_ascii_lowercase();

        let mut mods = Mods::default();
        let mut parts = lowered.split('+').peekable();
        let mut base_token = String::new();

        while let Some(part) = parts.next() {
            // A trailing `+` means the base *is* `+` -- `ctrl++` is Ctrl and the
            // plus key. Only the last segment is ever a base.
            let is_last = parts.peek().is_none();
            if is_last {
                base_token = if part.is_empty() && lowered.ends_with('+') {
                    "+".to_string()
                } else {
                    part.to_string()
                };
                break;
            }
            match part {
                "ctrl" | "control" => mods.ctrl = true,
                "alt" | "option" | "opt" => mods.alt = true,
                "shift" => mods.shift = true,
                "cmd" | "command" | "super" | "win" | "meta" => {
                    return Err(KeyError::Desktop(spelled.to_string()))
                }

                _ => return Err(KeyError::Unknown { name: spelled.to_string() }),
            }
        }

        let base = parse_base(&base_token, spelled, mods)?;
        Ok(Self { base, mods })
    }
}

fn parse_base(token: &str, spelled: &str, mods: Mods) -> Result<Base, KeyError> {
    let squashed: String = token.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).collect();

    if let Some((_, named)) = NAMES.iter().find(|(name, _)| *name == squashed) {
        return Ok(Base::Named(*named));
    }

    // `f1`-`f12`. Parsed rather than tabulated so `f13` refuses with the
    // vocabulary rather than reading as an unknown word.
    if let Some(rest) = squashed.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=12).contains(&n) {
                return Ok(Base::Named(Named::F(n)));
            }
            return Err(KeyError::Unknown { name: spelled.to_string() });
        }
    }

    // A single character. `ctrl+c` and `alt+b` are keys; a bare `c` is text, and
    // saying so keeps a model from spelling a word as a list of letters.
    //
    // Whitespace-trimmed rather than `squashed`: `_` and `-` are *themselves*
    // control chords -- `ctrl+_` is 0x1f and `ctrl+-` passes the character
    // through -- so stripping them here would refuse two chords that work.
    // Only spaces around a name are noise; inside it they are not.
    let single = token.trim();
    let mut chars = single.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if mods.shift {
            return Err(KeyError::ShiftedCharacter(spelled.to_string()));
        }
        if !mods.ctrl && !mods.alt {
            return Err(KeyError::BareCharacter(spelled.to_string()));
        }
        return Ok(Base::Char(ch));
    }

    Err(KeyError::Unknown { name: spelled.to_string() })
}

/// Whether encoding this chord needs the session's [`Modes`].
///
/// Only the six DECCKM-sensitive keys do. Everything else encodes the same
/// under any mode, which is what lets `input` keep typing free of an attach for
/// the calls that do not need one.
#[must_use]
pub fn needs_modes(chord: &Chord) -> bool {
    matches!(
        chord.base,
        Base::Named(
            Named::Up | Named::Down | Named::Left | Named::Right | Named::Home | Named::End
        )
    )
}

/// Encode one chord.
///
/// A case-for-case port of `zest_input::key::encode_legacy`. Held against it by
/// `tests/keys.rs`; see this module's header before changing anything here.
#[must_use]
pub fn encode(chord: &Chord, modes: Modes) -> Vec<u8> {
    let Chord { base, mods } = *chord;

    // The cursor-key prefix. DECCKM switches the whole family from CSI to SS3,
    // and applications set it expecting to be obeyed.
    let cursor: &[u8] = if modes.contains(Modes::APP_CURSOR) { b"\x1bO" } else { b"\x1b[" };

    match base {
        Base::Char(ch) => encode_text(&ch.to_string(), mods.ctrl, mods.alt),
        Base::Named(named) => match named {
            // The single-byte keys route through `finish` like text does --
            // Alt-as-ESC lives there, and arms that returned their byte
            // directly were how alt+backspace lost its ESC (#350). The CSI
            // arms below do not: their modifier parameter already carries Alt.
            Named::Enter => finish(vec![b'\r'], mods.alt),
            Named::Tab => {
                // CBT has no modifier slot, so ctrl+shift+tab used to be
                // byte-identical to shift+tab. CSI u is the only form with
                // somewhere to put the Ctrl -- it is what kitty, foot and
                // ghostty send for these chords even with the protocol off --
                // and its parameter carries Alt, so no ESC prefix on top.
                if mods.ctrl {
                    format!("\x1b[9;{}u", mods.param()).into_bytes()
                } else if mods.shift {
                    finish(b"\x1b[Z".to_vec(), mods.alt) // CBT, back-tab
                } else {
                    finish(vec![b'\t'], mods.alt)
                }
            }
            // Backspace is DEL (0x7f), not BS (0x08). Sending 0x08 is a classic
            // mistake that makes readline and most shells misbehave.
            Named::Backspace => finish(vec![if mods.ctrl { 0x08 } else { 0x7f }], mods.alt),
            Named::Esc => finish(vec![0x1b], mods.alt),

            Named::Up => with_mods(cursor, b'A', mods),
            Named::Down => with_mods(cursor, b'B', mods),
            Named::Right => with_mods(cursor, b'C', mods),
            Named::Left => with_mods(cursor, b'D', mods),

            // Home and End also follow DECCKM.
            Named::Home => with_mods(cursor, b'H', mods),
            Named::End => with_mods(cursor, b'F', mods),

            Named::PageUp => tilde(5, mods),
            Named::PageDown => tilde(6, mods),
            Named::Insert => tilde(2, mods),
            Named::Delete => tilde(3, mods),

            Named::F(1) => ss3_or_tilde(b'P', 11, mods),
            Named::F(2) => ss3_or_tilde(b'Q', 12, mods),
            Named::F(3) => ss3_or_tilde(b'R', 13, mods),
            Named::F(4) => ss3_or_tilde(b'S', 14, mods),
            Named::F(5) => tilde(15, mods),
            Named::F(6) => tilde(17, mods),
            Named::F(7) => tilde(18, mods),
            Named::F(8) => tilde(19, mods),
            Named::F(9) => tilde(20, mods),
            Named::F(10) => tilde(21, mods),
            Named::F(11) => tilde(23, mods),
            Named::F(12) => tilde(24, mods),
            // `F(n)` is constructed only by the parser, which bounds it to
            // 1..=12. Unreachable rather than silently empty.
            Named::F(_) => unreachable!("the parser admits only f1-f12"),

            Named::Space => encode_text(" ", mods.ctrl, mods.alt),
        },
    }
}

/// Printable text, with Ctrl and Alt applied.
fn encode_text(text: &str, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();

    if ctrl {
        // Ctrl maps a letter to its control code: Ctrl-A is 0x01. The
        // punctuation cases are the conventional ones every terminal implements.
        if let Some(ch) = text.chars().next() {
            let code = match ch.to_ascii_lowercase() {
                c @ 'a'..='z' => Some((c as u8) - b'a' + 1),
                '@' | ' ' => Some(0x00),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' | '?' => Some(0x1f),
                // Not a control combination -- pass the character through
                // rather than swallowing it.
                _ => None,
            };
            match code {
                Some(c) => out.push(c),
                None => out.extend_from_slice(text.as_bytes()),
            }
            return finish(out, alt);
        }
    }

    out.extend_from_slice(text.as_bytes());
    finish(out, alt)
}

/// Alt is sent as a leading ESC.
///
/// The alternative convention (setting the high bit) is long obsolete and
/// breaks UTF-8 outright.
fn finish(bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt && !bytes.is_empty() {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        return out;
    }
    bytes
}

/// A cursor-style key, with a modifier parameter when one applies.
///
/// Modified cursor keys always use CSI, never SS3, even under DECCKM -- SS3 has
/// no parameter slot to put the modifier in.
fn with_mods(prefix: &[u8], final_byte: u8, mods: Mods) -> Vec<u8> {
    let param = mods.param();
    if param == 1 {
        let mut v = prefix.to_vec();
        v.push(final_byte);
        return v;
    }
    format!("\x1b[1;{param}{}", final_byte as char).into_bytes()
}

/// A `CSI n ~` key.
fn tilde(n: u8, mods: Mods) -> Vec<u8> {
    let param = mods.param();
    if param == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{param}~").into_bytes()
    }
}

/// F1-F4 are SS3 unmodified and `CSI n ~` when modified.
///
/// Not an optimization: `CSI 1 ; m R` is CPR, so a modified F3 must not be
/// spelled the way an unmodified one is.
fn ss3_or_tilde(ss3: u8, tilde_n: u8, mods: Mods) -> Vec<u8> {
    if mods.param() == 1 {
        vec![0x1b, b'O', ss3]
    } else {
        tilde(tilde_n, mods)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        s.parse().unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    fn bytes(s: &str, modes: Modes) -> Vec<u8> {
        encode(&chord(s), modes)
    }

    const PLAIN: Modes = Modes::empty();

    #[test]
    fn a_name_parses_however_it_is_spelled() {
        // A model will not reproduce one canonical spelling reliably, and every
        // variation it does produce should work rather than teach it that the
        // tool is flaky.
        for spelled in ["pageup", "PageUp", "page_up", "page-up", "PAGE UP", "pgup", " pageup "] {
            assert_eq!(
                chord(spelled).base,
                Base::Named(Named::PageUp),
                "`{spelled}` must name PageUp"
            );
        }
        assert_eq!(chord("escape").base, chord("esc").base);
        assert_eq!(chord("return").base, chord("enter").base);
    }

    #[test]
    fn modifier_order_does_not_matter() {
        assert_eq!(chord("ctrl+shift+left"), chord("shift+ctrl+left"));
        assert_eq!(chord("CTRL+Left").mods, Mods { ctrl: true, ..Mods::default() });
    }

    #[test]
    fn an_unknown_name_is_refused_and_names_the_vocabulary() {
        // The whole point of the tool: a key that silently does nothing is
        // indistinguishable, inside an agent loop, from one the application
        // ignored. #345 measured exactly that.
        let e = "escpae".parse::<Chord>().expect_err("a typo must not parse");
        assert_eq!(e, KeyError::Unknown { name: "escpae".into() });
        let msg = e.to_string();
        assert!(msg.contains("pageup"), "the refusal must carry the vocabulary: {msg}");
        assert!(msg.contains("`text`"), "and say where characters go: {msg}");

        assert!("f13".parse::<Chord>().is_err(), "the reference encodes nothing above F12");
    }

    #[test]
    fn the_desktop_modifiers_are_refused_with_the_reason() {
        for spelled in ["cmd+c", "super+a", "win+e", "meta+x"] {
            let e = spelled.parse::<Chord>().expect_err("Super is not the terminal's");
            assert!(
                matches!(e, KeyError::Desktop(_)),
                "`{spelled}` must refuse as a desktop chord, got {e:?}"
            );
            assert!(e.to_string().contains("ctrl, alt and shift"), "and name what is: {e}");
        }
    }

    #[test]
    fn characters_are_sent_as_text_not_as_keys() {
        // Without this a model spells a word as `keys: ["h","i"]`, which works
        // and is the wrong shape -- and `shift+a` invites this table to invent
        // the character a layout produces, which it cannot do.
        assert!(matches!("a".parse::<Chord>(), Err(KeyError::BareCharacter(_))));
        assert!(matches!("shift+a".parse::<Chord>(), Err(KeyError::ShiftedCharacter(_))));
        assert!("ctrl+c".parse::<Chord>().is_ok(), "a control chord is a key");
        assert!("alt+b".parse::<Chord>().is_ok());
    }

    #[test]
    fn punctuation_chords_survive_the_separator_squashing() {
        // `_` and `-` are control chords in their own right, so the character
        // branch trims whitespace and nothing else. Squashing them the way a
        // *name* is squashed would refuse two chords that work.
        assert_eq!(bytes("ctrl+_", PLAIN), vec![0x1f]);
        assert_eq!(bytes("ctrl+[", PLAIN), vec![0x1b]);
        assert_eq!(bytes("ctrl+\\", PLAIN), vec![0x1c]);
        // ...while stray spaces around a character are just noise, as they are
        // around a name.
        assert_eq!(bytes("ctrl+ c", PLAIN), vec![0x03]);
    }

    #[test]
    fn ctrl_c_is_the_byte_interrupt_sends() {
        // `interrupt` keeps its own `ETX` const rather than routing one byte
        // through a string parser, so this is what stops the two drifting.
        assert_eq!(bytes("ctrl+c", PLAIN), vec![0x03]);
    }

    #[test]
    fn arrows_follow_decckm() {
        // The reason the encoding is server-side at all: the mode lives on the
        // host, so a caller writing the sequence itself is right about half the
        // time (#345).
        assert_eq!(bytes("up", PLAIN), b"\x1b[A");
        assert_eq!(bytes("up", Modes::APP_CURSOR), b"\x1bOA");
        assert_eq!(bytes("home", Modes::APP_CURSOR), b"\x1bOH");
        // Modified cursor keys are CSI even under DECCKM: SS3 has no parameter
        // slot to put the modifier in.
        assert_eq!(bytes("ctrl+up", Modes::APP_CURSOR), b"\x1b[1;5A");
    }

    #[test]
    fn shift_tab_is_back_tab() {
        // The chord #345 watched land in a composer as the literal text `[Z`.
        assert_eq!(bytes("shift+tab", PLAIN), b"\x1b[Z");
        assert_eq!(bytes("tab", PLAIN), b"\t");
    }

    #[test]
    fn alt_prefixes_the_named_single_byte_keys() {
        // The same rule as `alt+b`: Alt is a leading ESC. These went out bare
        // before #350, so alt+backspace deleted one character where readline,
        // zsh and fish delete a word.
        assert_eq!(bytes("alt+backspace", PLAIN), vec![0x1b, 0x7f]);
        assert_eq!(bytes("alt+enter", PLAIN), b"\x1b\r");
        assert_eq!(bytes("alt+esc", PLAIN), vec![0x1b, 0x1b]);
        assert_eq!(bytes("alt+tab", PLAIN), b"\x1b\t");
        assert_eq!(
            bytes("ctrl+alt+backspace", PLAIN),
            vec![0x1b, 0x08],
            "ctrl still flips backspace to BS, and alt prefixes that too"
        );
    }

    #[test]
    fn ctrl_tab_chords_escalate_to_csi_u() {
        // `\x1b[Z` has no modifier slot, so ctrl+shift+tab used to be
        // byte-identical to shift+tab and tmux-style tab cycling could not
        // bind the pair apart. (#350)
        assert_eq!(bytes("ctrl+tab", PLAIN), b"\x1b[9;5u");
        assert_eq!(bytes("ctrl+shift+tab", PLAIN), b"\x1b[9;6u");
    }

    #[test]
    fn the_table_ignores_kitty_flags() {
        // Stated rather than left to an absence. `zest-input` branches to CSI-u
        // when a program pushes flags; porting that would be a second large
        // table and the third-copy trap again. Legacy forms are accepted by
        // every program that speaks the kitty protocol -- its own rule.
        let kitty = Modes::empty().with_kitty_flags(0b1111);
        assert_eq!(bytes("up", kitty), b"\x1b[A", "flags must not change the encoding");
        assert_eq!(bytes("ctrl+c", kitty), vec![0x03]);
    }

    #[test]
    fn only_the_cursor_family_needs_the_session_modes() {
        // What lets `input` stay attach-free for the calls that do not need one.
        for s in ["up", "down", "left", "right", "home", "end"] {
            assert!(needs_modes(&chord(s)), "`{s}` is DECCKM-sensitive");
        }
        for s in ["enter", "esc", "tab", "ctrl+c", "f5", "pageup", "backspace"] {
            assert!(!needs_modes(&chord(s)), "`{s}` encodes the same under any mode");
        }
    }
}
