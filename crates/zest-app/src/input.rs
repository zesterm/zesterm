//! Keyboard events to terminal byte sequences.
//!
//! Lives here for now; it moves to `zest-input` when it grows a Kitty-protocol
//! path and mouse encoding. The shape is deliberately already
//! `(key, modifiers, modes) -> Option<Vec<u8>>` so that move is mechanical.
//!
//! The single most important thing here is that **modes change the encoding**.
//! Arrow keys are `ESC [ A` normally and `ESC O A` under DECCKM, and getting
//! that backwards breaks vim and readline in ways that look like the terminal
//! randomly ignoring the keyboard.

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use zest_core::Modes;

/// Encode a key press.
///
/// Returns `None` for keys that produce nothing — bare modifiers, unhandled
/// function keys — so the caller can distinguish "nothing to send" from "sent
/// empty".
pub fn encode(event: &KeyEvent, mods: ModifiersState, modes: Modes) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // The cursor-key prefix. DECCKM (`CSI ? 1 h`) switches the whole family from
    // CSI to SS3, and applications set it expecting to be obeyed.
    let cursor = if modes.contains(Modes::APP_CURSOR) { b"\x1bO" } else { b"\x1b[" };

    let bytes: Vec<u8> = match &event.logical_key {
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

/// Which mouse button, in the encoding's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    WheelUp = 64,
    WheelDown = 65,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    /// Movement with a button held.
    Drag,
    /// Movement with no button held.
    Motion,
}

/// Encode a mouse event for the program.
///
/// Returns `None` when the active mode does not report this event — a plain
/// motion with no button, say, unless the program asked for 1003.
///
/// Only SGR (mode 1006) is emitted. The legacy X10 encoding packs coordinates
/// into single bytes as `32 + n`, which cannot express a column past 223 — an
/// ordinary maximised terminal is wider than that, so on those columns the
/// legacy form reports the wrong cell or nothing at all. Every program that
/// wants mouse input on a modern terminal negotiates 1006.
#[must_use]
pub fn encode_mouse(
    button: MouseButton,
    action: MouseAction,
    row: usize,
    col: usize,
    mods: ModifiersState,
    modes: Modes,
) -> Option<Vec<u8>> {
    if !modes.mouse_enabled() {
        return None;
    }

    match action {
        MouseAction::Drag if !modes.intersects(Modes::MOUSE_DRAG | Modes::MOUSE_MOTION) => {
            return None
        }
        MouseAction::Motion if !modes.contains(Modes::MOUSE_MOTION) => return None,
        _ => {}
    }

    if !modes.contains(Modes::MOUSE_SGR) {
        // Without 1006 we would have to use the legacy encoding, whose
        // coordinates break past column 223. Reporting nothing is better than
        // reporting the wrong cell.
        return None;
    }

    let mut code = button as u32;
    if mods.shift_key() {
        code |= 4;
    }
    if mods.alt_key() {
        code |= 8;
    }
    if mods.control_key() {
        code |= 16;
    }
    if matches!(action, MouseAction::Drag | MouseAction::Motion) {
        code |= 32;
    }

    // SGR is 1-based, and encodes press versus release in the final byte rather
    // than losing which button was released the way the legacy form does.
    let final_byte = if action == MouseAction::Release { 'm' } else { 'M' };
    Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, final_byte).into_bytes())
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

    // --- mouse ---

    const SGR: Modes = Modes::MOUSE_SGR;

    fn click_modes() -> Modes {
        Modes::MOUSE_CLICK | SGR
    }

    #[test]
    fn no_mouse_events_when_the_program_did_not_ask() {
        // Without this, selection could never coexist with mouse reporting --
        // and more importantly, a program that never enabled a mouse mode would
        // receive garbage in its input stream.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, 3, 7, NONE, Modes::empty()),
            None
        );
    }

    #[test]
    fn sgr_press_and_release_are_distinguishable() {
        let press =
            encode_mouse(MouseButton::Left, MouseAction::Press, 3, 7, NONE, click_modes()).unwrap();
        assert_eq!(press, b"\x1b[<0;8;4M".to_vec(), "coordinates are 1-based");

        let release =
            encode_mouse(MouseButton::Left, MouseAction::Release, 3, 7, NONE, click_modes())
                .unwrap();
        assert_eq!(
            release,
            b"\x1b[<0;8;4m".to_vec(),
            "SGR encodes release in the final byte, so the button is not lost"
        );
    }

    #[test]
    fn buttons_use_the_conventional_numbering() {
        let enc = |b| encode_mouse(b, MouseAction::Press, 0, 0, NONE, click_modes()).unwrap();
        assert_eq!(enc(MouseButton::Left), b"\x1b[<0;1;1M".to_vec());
        assert_eq!(enc(MouseButton::Middle), b"\x1b[<1;1;1M".to_vec());
        assert_eq!(enc(MouseButton::Right), b"\x1b[<2;1;1M".to_vec());
        assert_eq!(enc(MouseButton::WheelUp), b"\x1b[<64;1;1M".to_vec());
        assert_eq!(enc(MouseButton::WheelDown), b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn modifiers_are_folded_into_the_button_code() {
        let enc = |m| encode_mouse(MouseButton::Left, MouseAction::Press, 0, 0, m, click_modes());
        assert_eq!(enc(ModifiersState::SHIFT).unwrap(), b"\x1b[<4;1;1M".to_vec());
        assert_eq!(enc(ModifiersState::ALT).unwrap(), b"\x1b[<8;1;1M".to_vec());
        assert_eq!(enc(ModifiersState::CONTROL).unwrap(), b"\x1b[<16;1;1M".to_vec());
    }

    #[test]
    fn drag_needs_1002_and_bare_motion_needs_1003() {
        // Reporting every mouse move to a program that only asked for clicks
        // floods its input and pegs the CPU.
        let drag = |modes| encode_mouse(MouseButton::Left, MouseAction::Drag, 1, 1, NONE, modes);
        assert_eq!(drag(click_modes()), None, "1000 alone does not report drags");
        assert!(drag(Modes::MOUSE_DRAG | SGR).is_some());

        let motion =
            |modes| encode_mouse(MouseButton::Left, MouseAction::Motion, 1, 1, NONE, modes);
        assert_eq!(motion(Modes::MOUSE_DRAG | SGR), None, "1002 does not report bare motion");
        assert!(motion(Modes::MOUSE_MOTION | SGR).is_some());
    }

    #[test]
    fn motion_sets_the_drag_bit() {
        let d = encode_mouse(
            MouseButton::Left,
            MouseAction::Drag,
            2,
            2,
            NONE,
            Modes::MOUSE_DRAG | SGR,
        )
        .unwrap();
        assert_eq!(d, b"\x1b[<32;3;3M".to_vec(), "bit 32 marks motion");
    }

    #[test]
    fn without_sgr_nothing_is_reported() {
        // The legacy encoding packs coordinates as 32+n, which cannot express a
        // column past 223 -- narrower than an ordinary maximised window. Sending
        // the wrong cell is worse than sending nothing.
        assert_eq!(
            encode_mouse(MouseButton::Left, MouseAction::Press, 0, 0, NONE, Modes::MOUSE_CLICK),
            None
        );
    }

    #[test]
    fn coordinates_survive_past_the_legacy_limit() {
        let far =
            encode_mouse(MouseButton::Left, MouseAction::Press, 10, 300, NONE, click_modes())
                .unwrap();
        assert_eq!(far, b"\x1b[<0;301;11M".to_vec(), "column 301 is representable");
    }

    #[test]
    fn empty_input_produces_nothing() {
        // Distinguishing "nothing to send" from "send empty" keeps bare modifier
        // presses from waking the pty.
        assert_eq!(encode_text("", false, false), None);
        assert_eq!(encode_text("", false, true), None, "even with Alt");
    }
}
