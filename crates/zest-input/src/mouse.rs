//! Mouse events to terminal byte sequences.
//!
//! Only SGR (mode 1006) is emitted, and that is a deliberate omission rather
//! than an unfinished one — see [`encode_mouse`].

use winit::keyboard::ModifiersState;
use zest_core::Modes;

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
#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();

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
}
