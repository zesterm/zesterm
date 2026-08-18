//! Keyboard events to terminal byte sequences.
//!
//! `(key, modifiers, modes) -> Option<Vec<u8>>`. `None` means *nothing to send*,
//! which is not the same as sending nothing: a bare modifier press must not wake
//! the pty at all.
//!
//! The Kitty keyboard protocol (CSI u) lives here too, behind the flags the
//! program pushed with `CSI > flags u`. Its disambiguation model is not
//! expressible as a tweak to the legacy scheme, so the two paths are separate
//! and the legacy one is untouched when no flags are set — which is the case
//! for every program that has not asked.
//!
//! # Why the encoding happens on the client
//!
//! The flags are set by the program, which is on the host, and applied to a
//! keystroke, which is on this machine. They reach here inside `Modes`, over
//! the same wire as autowrap and the alternate screen. See `zest_core::modes`.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use zest_core::Modes;

/// Whether a chord belongs to the desktop rather than to the terminal.
///
/// The terminal modifier set is Ctrl, Alt and Shift. There is no escape
/// sequence for Super/Command and no application expects one, so a chord that
/// includes it is the window manager's or the app's — never the shell's.
#[must_use]
pub fn belongs_to_desktop(mods: ModifiersState) -> bool {
    mods.super_key()
}

/// Whether this chord means copy/paste.
///
/// Two conventions, both accepted everywhere. `Ctrl+Shift+C/V` is what
/// terminals use on Windows and Linux, and it exists only because plain
/// `Ctrl+C` must stay SIGINT. macOS has no such conflict and universally uses
/// Command — a terminal accepting only `Ctrl+Shift` there is one where paste
/// appears not to work at all.
///
/// Accepting both rather than choosing by `cfg`: the other chord is unused on
/// each platform, and accepting it costs nothing against someone's muscle
/// memory from another machine.
#[must_use]
pub fn is_clipboard_chord(mods: ModifiersState) -> bool {
    mods.super_key() || (mods.control_key() && mods.shift_key())
}

/// Which of press, repeat and release this is.
///
/// The legacy scheme cannot express the distinction at all — it is the reason
/// kitty flag 2 exists — so this is carried separately rather than folded into
/// the modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventType {
    Press,
    Repeat,
    Release,
}

impl EventType {
    fn of(state: ElementState, repeat: bool) -> Self {
        match (state, repeat) {
            (ElementState::Released, _) => EventType::Release,
            (ElementState::Pressed, true) => EventType::Repeat,
            (ElementState::Pressed, false) => EventType::Press,
        }
    }

    /// The protocol's sub-parameter. Press is 1 and is written as the default.
    fn param(self) -> u8 {
        match self {
            EventType::Press => 1,
            EventType::Repeat => 2,
            EventType::Release => 3,
        }
    }
}

/// A keystroke, reduced to what the encoders actually need.
///
/// Everything below is written against this rather than `winit::KeyEvent`,
/// which has no public constructor worth using and cannot be built in a test.
/// The public [`encode`] is the only place the two meet.
struct KeyPress<'a> {
    key: &'a Key,
    location: KeyLocation,
    mods: ModifiersState,
    event: EventType,
}

/// Encode a key event.
///
/// Returns `None` for keys that produce nothing — bare modifiers, unhandled
/// function keys, anything the desktop has claimed — so the caller can
/// distinguish "nothing to send" from "sent empty".
///
/// Releases and repeats reach the pty only when the program asked for them with
/// kitty flag 2; otherwise a release encodes to `None` exactly as before.
pub fn encode(event: &KeyEvent, mods: ModifiersState, modes: Modes) -> Option<Vec<u8>> {
    KeyPress {
        key: &event.logical_key,
        location: event.location,
        mods,
        event: EventType::of(event.state, event.repeat),
    }
    .encode(modes)
}

/// Encode a key press from its parts, for a caller with no `KeyEvent`.
///
/// [`KeyEvent`] has a private platform tail and cannot be constructed outside
/// winit -- this file's own tests say so and route around it through
/// `KeyPress`. So a consumer that must be held *against* this encoder had no
/// way to reach it, and `zest-mcp`'s `keys.rs` is exactly that: a third
/// implementation of one table, after `clients/web/packages/input/src/key.ts`.
/// It is pinned byte-for-byte by `zest-mcp/tests/keys.rs`, which calls this.
///
/// **Not a second path.** It builds the same [`KeyPress`] [`encode`] builds, so
/// a divergence between the two is impossible by construction rather than by
/// review.
///
/// Presses only. Releases and repeats are expressible only under the kitty
/// protocol, which needs the whole `KeyEvent` to say which one it was.
#[must_use]
pub fn encode_press(
    key: &Key,
    location: KeyLocation,
    mods: ModifiersState,
    modes: Modes,
) -> Option<Vec<u8>> {
    KeyPress { key, location, mods, event: EventType::Press }.encode(modes)
}

impl KeyPress<'_> {
    fn encode(&self, modes: Modes) -> Option<Vec<u8>> {
        // Falling through with Super held would encode the *unmodified* key, so
        // on macOS every Command shortcut types its own letter: Cmd+V pastes
        // nothing and inserts a `v`. `None` leaves the chord to the caller,
        // which is where the clipboard shortcuts live.
        if belongs_to_desktop(self.mods) {
            return None;
        }

        let flags = modes.kitty_flags();
        if flags != 0 {
            return self.encode_kitty(flags);
        }

        // Nothing asked for event types, so a release is not encodable and a
        // repeat is indistinguishable from a press -- which is what every
        // program not speaking this protocol expects.
        if self.event == EventType::Release {
            return None;
        }
        encode_legacy(self.key, self.mods, modes)
    }

    /// Encode under the Kitty keyboard protocol.
    ///
    /// `flags` is what the program pushed, already masked to what this terminal
    /// implements — see `zest_core::Modes::kitty_flags`.
    fn encode_kitty(&self, flags: u8) -> Option<Vec<u8>> {
        let event_types = flags & 2 != 0;
        let report_all = flags & 8 != 0;

        // Nothing asked to hear about releases, so there is no form to send one
        // in. A repeat still encodes, as a press -- which is what a program
        // holding a key down has always seen.
        if !event_types && self.event == EventType::Release {
            return None;
        }

        match self.key {
            Key::Named(named) => self.encode_kitty_named(*named, report_all, event_types),
            Key::Character(text) => self.encode_kitty_text(text, report_all, event_types),
            _ => None,
        }
    }

    fn encode_kitty_named(
        &self,
        named: NamedKey,
        report_all: bool,
        event_types: bool,
    ) -> Option<Vec<u8>> {
        let (mods, ev) = (self.mods, self.event);

        // Escape is always the escape form, modified or not. It is the reason
        // to turn flag 1 on at all: a bare `0x1b` is indistinguishable from the
        // first byte of every other sequence, so an application has to guess
        // with a timer, and `CSI 27 u` is what removes the guess.
        if named == NamedKey::Escape {
            return Some(kitty_u(27, mods, ev, event_types));
        }

        // The other keys with a legacy byte keep it, because disambiguating
        // `Ctrl+I` from `Tab` does not require changing what `Tab` itself
        // sends -- and changing it would break every program that asked only to
        // disambiguate. Flag 8 is the program asking for the escape code
        // regardless.
        if let Some((number, legacy)) = legacy_byte(named) {
            let plain = modifier_param(mods) == 1 && !(event_types && ev.param() != 1);
            // `plain` is false for anything with an event type to report, so a
            // release always escalates to the escape form here rather than
            // falling through to a legacy byte that cannot carry one. Unlike a
            // text key, these have a protocol number to be reported under.
            return Some(if report_all || !plain {
                kitty_u(number, mods, ev, event_types)
            } else {
                legacy
            });
        }

        // Cursor and F1-F4 keep their CSI letter, never SS3: SS3 has no
        // parameter slot, and a program that asked to disambiguate is owed a
        // form that can carry the modifiers. DECCKM is deliberately ignored
        // here for the same reason, which is why `modes` is not a parameter.
        if let Some(letter) = csi_letter(named) {
            return Some(kitty_letter(letter, mods, ev, event_types));
        }
        if let Some(n) = tilde_number(named) {
            return Some(kitty_tilde(n, mods, ev, event_types));
        }

        // The modifier keys themselves. Only flag 8 asks for these, and without
        // it a bare modifier press must stay silent rather than wake the pty.
        if report_all {
            if let Some(n) = modifier_key_number(named, self.location) {
                return Some(kitty_u(n, mods, ev, event_types));
            }
        }
        None
    }

    fn encode_kitty_text(
        &self,
        text: &str,
        report_all: bool,
        event_types: bool,
    ) -> Option<Vec<u8>> {
        let ch = text.chars().next()?;
        // The protocol keys on the *unshifted* character. Recovering it
        // properly is kitty flag 4 (report alternate keys), which this terminal
        // does not implement, so lowercasing is the closest honest answer and is
        // exact for the letters that actually collide.
        let number = ch.to_lowercase().next().unwrap_or(ch) as u32;

        // Plain typing stays plain text even under this protocol -- the program
        // wants the character, not a description of the key. It is Ctrl and Alt
        // that collapse distinct keys onto one byte, and those are what the
        // escape form exists to separate.
        let ambiguous = self.mods.control_key() || self.mods.alt_key();
        if report_all || ambiguous {
            return Some(kitty_u(number, self.mods, self.event, event_types));
        }

        // No escape form for this key, so there is nowhere to put an event
        // type: the release of a key whose press was the byte `a` is not
        // expressible, and sending `a` again would type it twice. Flag 2
        // without flag 8 therefore reports releases for modified and functional
        // keys only, which is the protocol working as specified rather than a
        // gap. A *repeat* still sends the text, because a held key has always
        // typed.
        if self.event == EventType::Release {
            return None;
        }
        encode_text(text, false, false)
    }
}

/// The keys that have a single legacy byte, with their protocol numbers.
///
/// Escape is handled before this and is deliberately absent: it never keeps its
/// legacy byte under this protocol.
fn legacy_byte(named: NamedKey) -> Option<(u32, Vec<u8>)> {
    Some(match named {
        NamedKey::Enter => (13, vec![b'\r']),
        NamedKey::Tab => (9, vec![b'\t']),
        NamedKey::Backspace => (127, vec![0x7f]),
        NamedKey::Space => (32, vec![b' ']),
        _ => return None,
    })
}

/// The keys encoded as `CSI 1 ; mods letter`.
fn csi_letter(named: NamedKey) -> Option<u8> {
    Some(match named {
        NamedKey::ArrowUp => b'A',
        NamedKey::ArrowDown => b'B',
        NamedKey::ArrowRight => b'C',
        NamedKey::ArrowLeft => b'D',
        NamedKey::Home => b'H',
        NamedKey::End => b'F',
        _ => return None,
    })
}

/// The keys encoded as `CSI n ; mods ~`.
///
/// F1-F4 are here rather than in [`csi_letter`], where their `P`/`Q`/`R`/`S`
/// finals would be shorter: `CSI 1 ; m R` is CPR, the cursor position report.
/// The legacy encoder makes the same choice the moment F3 is modified, for the
/// same reason -- see `ss3_or_tilde`.
fn tilde_number(named: NamedKey) -> Option<u8> {
    Some(match named {
        NamedKey::Insert => 2,
        NamedKey::Delete => 3,
        NamedKey::PageUp => 5,
        NamedKey::PageDown => 6,
        NamedKey::F1 => 11,
        NamedKey::F2 => 12,
        NamedKey::F3 => 13,
        NamedKey::F4 => 14,
        NamedKey::F5 => 15,
        NamedKey::F6 => 17,
        NamedKey::F7 => 18,
        NamedKey::F8 => 19,
        NamedKey::F9 => 20,
        NamedKey::F10 => 21,
        NamedKey::F11 => 23,
        NamedKey::F12 => 24,
        _ => return None,
    })
}

/// The protocol's numbers for the modifier keys themselves.
///
/// Super is absent on purpose: [`belongs_to_desktop`] refuses the chord before
/// this is reached, so listing it would be a line that can never run.
fn modifier_key_number(named: NamedKey, location: KeyLocation) -> Option<u32> {
    let right = location == KeyLocation::Right;
    Some(match named {
        NamedKey::Shift if right => 57447,
        NamedKey::Shift => 57441,
        NamedKey::Control if right => 57448,
        NamedKey::Control => 57442,
        NamedKey::Alt if right => 57449,
        NamedKey::Alt => 57443,
        NamedKey::CapsLock => 57358,
        NamedKey::NumLock => 57360,
        _ => return None,
    })
}

/// The `mods:event` parameter, or `None` when both are at their defaults.
fn kitty_params(mods: ModifiersState, event: EventType, event_types: bool) -> Option<String> {
    let m = modifier_param(mods);
    let e = event.param();
    if event_types && e != 1 {
        Some(format!("{m}:{e}"))
    } else if m != 1 {
        Some(format!("{m}"))
    } else {
        None
    }
}

/// `CSI number ; mods:event u`.
///
/// The number is never omitted: `CSI u` with no parameter is SCORC.
fn kitty_u(number: u32, mods: ModifiersState, event: EventType, event_types: bool) -> Vec<u8> {
    match kitty_params(mods, event, event_types) {
        Some(p) => format!("\x1b[{number};{p}u").into_bytes(),
        None => format!("\x1b[{number}u").into_bytes(),
    }
}

/// `CSI 1 ; mods:event letter`, collapsing to `CSI letter` when it can.
fn kitty_letter(
    final_byte: u8,
    mods: ModifiersState,
    event: EventType,
    event_types: bool,
) -> Vec<u8> {
    let f = final_byte as char;
    match kitty_params(mods, event, event_types) {
        Some(p) => format!("\x1b[1;{p}{f}").into_bytes(),
        None => format!("\x1b[{f}").into_bytes(),
    }
}

/// `CSI n ; mods:event ~`.
fn kitty_tilde(n: u8, mods: ModifiersState, event: EventType, event_types: bool) -> Vec<u8> {
    match kitty_params(mods, event, event_types) {
        Some(p) => format!("\x1b[{n};{p}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// Encode a key press the way terminals did before CSI u.
fn encode_legacy(key: &Key, mods: ModifiersState, modes: Modes) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // The cursor-key prefix. DECCKM (`CSI ? 1 h`) switches the whole family from
    // CSI to SS3, and applications set it expecting to be obeyed.
    let cursor = if modes.contains(Modes::APP_CURSOR) { b"\x1bO" } else { b"\x1b[" };

    let bytes: Vec<u8> = match key {
        Key::Named(named) => match named {
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Tab => {
                if shift {
                    b"\x1b[Z".to_vec() // CBT, back-tab
                } else {
                    vec![b'\t']
                }
            }
            // Backspace is DEL (0x7f), not BS (0x08). Sending 0x08 is a classic
            // mistake that makes readline and most shells misbehave.
            NamedKey::Backspace => {
                if ctrl {
                    vec![0x08]
                } else {
                    vec![0x7f]
                }
            }
            NamedKey::Escape => vec![0x1b],

            NamedKey::ArrowUp => with_mods(cursor, b'A', mods),
            NamedKey::ArrowDown => with_mods(cursor, b'B', mods),
            NamedKey::ArrowRight => with_mods(cursor, b'C', mods),
            NamedKey::ArrowLeft => with_mods(cursor, b'D', mods),

            // Home and End also follow DECCKM.
            NamedKey::Home => with_mods(cursor, b'H', mods),
            NamedKey::End => with_mods(cursor, b'F', mods),

            NamedKey::PageUp => tilde(5, mods),
            NamedKey::PageDown => tilde(6, mods),
            NamedKey::Insert => tilde(2, mods),
            NamedKey::Delete => tilde(3, mods),

            NamedKey::F1 => ss3_or_tilde(b'P', 11, mods),
            NamedKey::F2 => ss3_or_tilde(b'Q', 12, mods),
            NamedKey::F3 => ss3_or_tilde(b'R', 13, mods),
            NamedKey::F4 => ss3_or_tilde(b'S', 14, mods),
            NamedKey::F5 => tilde(15, mods),
            NamedKey::F6 => tilde(17, mods),
            NamedKey::F7 => tilde(18, mods),
            NamedKey::F8 => tilde(19, mods),
            NamedKey::F9 => tilde(20, mods),
            NamedKey::F10 => tilde(21, mods),
            NamedKey::F11 => tilde(23, mods),
            NamedKey::F12 => tilde(24, mods),

            NamedKey::Space => encode_text(" ", ctrl, alt)?,
            _ => return None,
        },

        Key::Character(text) => encode_text(text, ctrl, alt)?,

        _ => return None,
    };

    Some(bytes)
}

/// Printable text, with Ctrl and Alt applied.
fn encode_text(text: &str, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();

    if ctrl {
        // Ctrl maps a letter to its control code: Ctrl-A is 0x01. The
        // punctuation cases are the conventional ones every terminal implements.
        let ch = text.chars().next()?;
        let code = match ch.to_ascii_lowercase() {
            c @ 'a'..='z' => (c as u8) - b'a' + 1,
            '@' | ' ' => 0x00,
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' | '?' => 0x1f,
            // Not a control combination -- pass the character through rather
            // than swallowing it.
            _ => {
                out.extend_from_slice(text.as_bytes());
                return finish(out, alt);
            }
        };
        out.push(code);
        return finish(out, alt);
    }

    out.extend_from_slice(text.as_bytes());
    finish(out, alt)
}

/// Alt is sent as a leading ESC.
///
/// The alternative convention (setting the high bit) is long obsolete and breaks
/// UTF-8 outright.
fn finish(bytes: Vec<u8>, alt: bool) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        Some(out)
    } else {
        Some(bytes)
    }
}


/// The xterm modifier parameter: 1 + bitmask.
fn modifier_param(mods: ModifiersState) -> u8 {
    let mut m = 0;
    if mods.shift_key() {
        m |= 1;
    }
    if mods.alt_key() {
        m |= 2;
    }
    if mods.control_key() {
        m |= 4;
    }
    m + 1
}

/// A cursor-style key, with a modifier parameter when one applies.
///
/// Modified cursor keys always use CSI, never SS3, even under DECCKM — SS3 has
/// no parameter slot to put the modifier in.
fn with_mods(prefix: &[u8], final_byte: u8, mods: ModifiersState) -> Vec<u8> {
    let param = modifier_param(mods);
    if param == 1 {
        let mut v = prefix.to_vec();
        v.push(final_byte);
        return v;
    }
    format!("\x1b[1;{param}{}", final_byte as char).into_bytes()
}

/// A `CSI n ~` key.
fn tilde(n: u8, mods: ModifiersState) -> Vec<u8> {
    let param = modifier_param(mods);
    if param == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{param}~").into_bytes()
    }
}

/// F1-F4 are SS3 unmodified and `CSI n ~` when modified.
fn ss3_or_tilde(ss3: u8, tilde_n: u8, mods: ModifiersState) -> Vec<u8> {
    if modifier_param(mods) == 1 {
        vec![0x1b, b'O', ss3]
    } else {
        tilde(tilde_n, mods)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn command_chords_belong_to_the_desktop() {
        // Without this the terminal encodes the bare key, and on macOS every
        // Cmd shortcut types its own letter -- Cmd+V inserting a `v` is how
        // this was found.
        assert!(belongs_to_desktop(ModifiersState::SUPER));
        assert!(belongs_to_desktop(ModifiersState::SUPER | ModifiersState::SHIFT));
        assert!(!belongs_to_desktop(NONE));
        assert!(!belongs_to_desktop(ModifiersState::CONTROL), "Ctrl is the terminal's");
        assert!(!belongs_to_desktop(ModifiersState::ALT), "Alt is the terminal's");
    }

    #[test]
    fn both_clipboard_conventions_are_accepted() {
        assert!(is_clipboard_chord(ModifiersState::SUPER), "macOS uses Command");
        assert!(
            is_clipboard_chord(ModifiersState::CONTROL | ModifiersState::SHIFT),
            "Windows and Linux use Ctrl+Shift"
        );
    }

    #[test]
    fn plain_control_is_never_a_clipboard_chord() {
        // The whole reason the Ctrl+Shift convention exists: Ctrl+C is SIGINT,
        // and a terminal that copied instead would be unusable.
        assert!(!is_clipboard_chord(ModifiersState::CONTROL));
        assert!(!is_clipboard_chord(ModifiersState::SHIFT));
        assert!(!is_clipboard_chord(NONE));
    }

    #[test]
    fn ctrl_letters_map_to_control_codes() {
        assert_eq!(encode_text("c", true, false), Some(vec![0x03]), "Ctrl-C is ETX");
        assert_eq!(encode_text("a", true, false), Some(vec![0x01]));
        assert_eq!(encode_text("d", true, false), Some(vec![0x04]));
        // Case-insensitive: Ctrl-Shift-C still produces 0x03.
        assert_eq!(encode_text("C", true, false), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_punctuation_uses_the_conventional_codes() {
        assert_eq!(encode_text("[", true, false), Some(vec![0x1b]), "Ctrl-[ is ESC");
        assert_eq!(encode_text(" ", true, false), Some(vec![0x00]), "Ctrl-Space is NUL");
        assert_eq!(encode_text("\\", true, false), Some(vec![0x1c]));
    }

    #[test]
    fn ctrl_with_an_unmapped_key_passes_the_character_through() {
        // Swallowing it would make Ctrl-1 do nothing at all, which reads as the
        // terminal ignoring the keyboard.
        assert_eq!(encode_text("1", true, false), Some(vec![b'1']));
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(encode_text("b", false, true), Some(vec![0x1b, b'b']));
        // Combined with Ctrl: ESC then the control code.
        assert_eq!(encode_text("c", true, true), Some(vec![0x1b, 0x03]));
    }

    #[test]
    fn plain_text_passes_through_as_utf8() {
        assert_eq!(encode_text("a", false, false), Some(vec![b'a']));
        assert_eq!(encode_text("é", false, false), Some("é".as_bytes().to_vec()));
        assert_eq!(encode_text("世", false, false), Some("世".as_bytes().to_vec()));
    }

    #[test]
    fn arrow_keys_follow_decckm() {
        // This is the one that breaks vim when it is wrong.
        let normal = with_mods(b"\x1b[", b'A', NONE);
        assert_eq!(normal, b"\x1b[A".to_vec(), "CSI form by default");

        let app = with_mods(b"\x1bO", b'A', NONE);
        assert_eq!(app, b"\x1bOA".to_vec(), "SS3 form under DECCKM");
    }

    #[test]
    fn modified_cursor_keys_always_use_csi() {
        // SS3 has no parameter slot, so a modified key must fall back to CSI
        // even when DECCKM is set.
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(with_mods(b"\x1bO", b'C', ctrl), b"\x1b[1;5C".to_vec());
        assert_eq!(with_mods(b"\x1b[", b'C', ctrl), b"\x1b[1;5C".to_vec());
    }

    #[test]
    fn modifier_parameters_match_xterm() {
        assert_eq!(modifier_param(NONE), 1);
        assert_eq!(modifier_param(ModifiersState::SHIFT), 2);
        assert_eq!(modifier_param(ModifiersState::ALT), 3);
        assert_eq!(modifier_param(ModifiersState::CONTROL), 5);
        assert_eq!(modifier_param(ModifiersState::CONTROL | ModifiersState::SHIFT), 6);
        assert_eq!(
            modifier_param(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SHIFT),
            8
        );
    }

    #[test]
    fn function_keys_switch_form_when_modified() {
        assert_eq!(ss3_or_tilde(b'P', 11, NONE), b"\x1bOP".to_vec(), "F1 is SS3 P");
        assert_eq!(
            ss3_or_tilde(b'P', 11, ModifiersState::SHIFT),
            b"\x1b[11;2~".to_vec(),
            "modified F1 needs a parameter, so CSI"
        );
    }

    #[test]
    fn navigation_keys_use_the_tilde_form() {
        assert_eq!(tilde(5, NONE), b"\x1b[5~".to_vec(), "PageUp");
        assert_eq!(tilde(3, NONE), b"\x1b[3~".to_vec(), "Delete");
        assert_eq!(tilde(3, ModifiersState::CONTROL), b"\x1b[3;5~".to_vec());
    }

    #[test]
    fn empty_input_produces_nothing() {
        // Distinguishing "nothing to send" from "send empty" keeps bare modifier
        // presses from waking the pty.
        assert_eq!(encode_text("", false, false), None);
        assert_eq!(encode_text("", false, true), None, "even with Alt");
    }

    // --- the Kitty keyboard protocol -------------------------------------

    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;

    /// The modes a program gets after pushing `flags`.
    fn kitty(flags: u8) -> Modes {
        Modes::initial().with_kitty_flags(flags)
    }

    /// Encode a named key. `KeyEvent` has a private platform tail and cannot be
    /// built in a test, which is why every decision lives on `KeyPress`.
    fn named(key: NamedKey, mods: ModifiersState, event: EventType, modes: Modes) -> Option<String> {
        let key = Key::Named(key);
        KeyPress { key: &key, location: KeyLocation::Standard, mods, event }
            .encode(modes)
            .map(|b| String::from_utf8(b).expect("an encoding is valid UTF-8"))
    }

    /// Encode a character key.
    fn text(s: &str, mods: ModifiersState, event: EventType, modes: Modes) -> Option<String> {
        let key = Key::Character(s.into());
        KeyPress { key: &key, location: KeyLocation::Standard, mods, event }
            .encode(modes)
            .map(|b| String::from_utf8(b).expect("an encoding is valid UTF-8"))
    }

    #[test]
    fn no_flags_means_the_legacy_encoding_exactly() {
        // The whole protocol is opt-in. A program that never asked must see
        // byte-for-byte what it saw before this existed.
        let plain = Modes::initial();
        assert_eq!(named(NamedKey::Tab, NONE, EventType::Press, plain).as_deref(), Some("\t"));
        assert_eq!(named(NamedKey::Escape, NONE, EventType::Press, plain).as_deref(), Some("\x1b"));
        assert_eq!(text("i", CTRL, EventType::Press, plain), Some("\u{9}".to_string()));
    }

    #[test]
    fn ctrl_i_stops_being_tab_under_flag_1() {
        // The headline of the whole protocol: both are 0x09 in the legacy
        // scheme, so no program could ever bind them separately.
        let m = kitty(1);
        assert_eq!(text("i", CTRL, EventType::Press, m).as_deref(), Some("\x1b[105;5u"));
        assert_eq!(named(NamedKey::Tab, NONE, EventType::Press, m).as_deref(), Some("\t"));
    }

    #[test]
    fn escape_always_takes_the_escape_form_under_flag_1() {
        // A bare 0x1b is indistinguishable from the first byte of every other
        // sequence, so applications guess with a timer. This is what ends that.
        let m = kitty(1);
        assert_eq!(named(NamedKey::Escape, NONE, EventType::Press, m).as_deref(), Some("\x1b[27u"));
    }

    #[test]
    fn enter_and_tab_keep_their_bytes_until_a_modifier_needs_saying() {
        // Changing what unmodified Enter sends would break every program that
        // asked only to disambiguate.
        let m = kitty(1);
        assert_eq!(named(NamedKey::Enter, NONE, EventType::Press, m).as_deref(), Some("\r"));
        assert_eq!(
            named(NamedKey::Enter, SHIFT, EventType::Press, m).as_deref(),
            Some("\x1b[13;2u"),
            "Shift+Enter has no legacy byte at all -- this is why the protocol exists"
        );
    }

    #[test]
    fn flag_8_reports_the_keys_that_have_bytes_as_escape_codes_too() {
        let m = kitty(1 | 8);
        assert_eq!(named(NamedKey::Enter, NONE, EventType::Press, m).as_deref(), Some("\x1b[13u"));
        assert_eq!(named(NamedKey::Tab, NONE, EventType::Press, m).as_deref(), Some("\x1b[9u"));
        assert_eq!(text("a", NONE, EventType::Press, m).as_deref(), Some("\x1b[97u"));
    }

    #[test]
    fn the_reported_code_point_is_the_unshifted_key() {
        // winit hands over "A" for Shift+a. A program keyed on 97 sees nothing
        // if 65 is sent, and the failure is silent and total.
        let m = kitty(1 | 8);
        assert_eq!(text("A", SHIFT, EventType::Press, m).as_deref(), Some("\x1b[97;2u"));
    }

    #[test]
    fn plain_typing_is_still_plain_text() {
        // Under flags 1 and 2 a program wants the character, not a description
        // of the key that produced it.
        let m = kitty(1 | 2);
        assert_eq!(text("a", NONE, EventType::Press, m).as_deref(), Some("a"));
        assert_eq!(text("A", SHIFT, EventType::Press, m).as_deref(), Some("A"));
    }

    #[test]
    fn event_types_are_only_reported_when_asked_for() {
        assert_eq!(
            named(NamedKey::ArrowUp, NONE, EventType::Release, kitty(1)),
            None,
            "a release has no form until flag 2 provides one"
        );
        assert_eq!(
            named(NamedKey::ArrowUp, NONE, EventType::Release, kitty(1 | 2)).as_deref(),
            Some("\x1b[1;1:3A"),
            "the modifier slot is still written, because the event type follows it"
        );
        assert_eq!(
            named(NamedKey::ArrowUp, NONE, EventType::Repeat, kitty(1 | 2)).as_deref(),
            Some("\x1b[1;1:2A")
        );
    }

    #[test]
    fn a_key_with_no_escape_form_reports_no_release() {
        // There is no way to say "the key whose press was the byte `a` came
        // back up", and sending `a` again would type it twice.
        let m = kitty(1 | 2);
        assert_eq!(text("a", NONE, EventType::Release, m), None);
        // A repeat is not a release: a held key has always typed.
        assert_eq!(text("a", NONE, EventType::Repeat, m).as_deref(), Some("a"));
        // Enter is not a text key. It has a protocol number, so its release has
        // somewhere to be reported and escalates out of the legacy byte.
        assert_eq!(
            named(NamedKey::Enter, NONE, EventType::Release, m).as_deref(),
            Some("\x1b[13;1:3u")
        );
    }

    #[test]
    fn cursor_keys_never_use_ss3_under_the_protocol() {
        // SS3 has no parameter slot, so it cannot carry a modifier or an event
        // type -- and a program that asked to disambiguate is owed a form that
        // can. DECCKM is deliberately ignored here.
        let m = Modes::initial().union(Modes::APP_CURSOR).with_kitty_flags(1);
        assert_eq!(named(NamedKey::ArrowUp, NONE, EventType::Press, m).as_deref(), Some("\x1b[A"));
        assert_eq!(
            named(NamedKey::ArrowUp, CTRL, EventType::Press, m).as_deref(),
            Some("\x1b[1;5A")
        );
    }

    #[test]
    fn f3_uses_the_tilde_form_because_csi_r_is_cpr() {
        // `CSI 1;m R` is the cursor position report. A terminal that sent it
        // for F3 would have programs answering their own keystrokes.
        let m = kitty(1);
        assert_eq!(named(NamedKey::F3, NONE, EventType::Press, m).as_deref(), Some("\x1b[13~"));
        assert_eq!(named(NamedKey::F1, CTRL, EventType::Press, m).as_deref(), Some("\x1b[11;5~"));
    }

    #[test]
    fn a_bare_modifier_stays_silent_until_flag_8() {
        // Waking the pty on every Shift press is the invariant the whole
        // encoder is built around.
        assert_eq!(named(NamedKey::Shift, SHIFT, EventType::Press, kitty(1)), None);
        assert_eq!(
            named(NamedKey::Shift, SHIFT, EventType::Press, kitty(1 | 8)).as_deref(),
            Some("\x1b[57441;2u")
        );
    }

    #[test]
    fn command_chords_are_still_the_desktops_under_every_flag() {
        // The protocol does not get to claim Cmd+V. Without this, macOS pastes
        // nothing and types a `v` -- the bug that put `belongs_to_desktop` in.
        let m = kitty(1 | 2 | 8);
        assert_eq!(text("v", ModifiersState::SUPER, EventType::Press, m), None);
    }
}
