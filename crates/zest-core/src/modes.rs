//! Terminal modes.
//!
//! Modes are *state the client sets and the frontend must respect*, which makes
//! them part of the remote protocol rather than an internal detail: the phone
//! has to know whether it is in the alternate screen (render a grid) or not
//! (render command blocks), and it has to know the mouse mode to encode taps
//! correctly.

bitflags::bitflags! {
    /// DEC private and ANSI modes that affect rendering or input encoding.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Modes: u32 {
        /// DECAWM (7) — autowrap. On by default.
        const AUTO_WRAP          = 1 << 0;
        /// DECOM (6) — origin mode: cursor addressing is relative to the
        /// scroll region rather than the screen.
        const ORIGIN             = 1 << 1;
        /// DECCKM (1) — application cursor keys. Changes arrow keys from
        /// `ESC [ A` to `ESC O A`; getting this wrong breaks vim and readline.
        const APP_CURSOR         = 1 << 2;
        /// DECNKM (66) — application keypad.
        const APP_KEYPAD         = 1 << 3;
        /// DECTCEM (25) — cursor visible.
        const SHOW_CURSOR        = 1 << 4;
        /// IRM (4) — insert rather than replace.
        const INSERT             = 1 << 5;
        /// LNM (20) — newline also returns the carriage.
        const LINE_FEED_NEW_LINE = 1 << 6;
        /// 1049 — the alternate screen.
        ///
        /// The single most useful bit for the mobile client: it is exactly the
        /// signal for "this program is painting a full screen", and therefore
        /// for when to switch from the block list to a grid view.
        const ALT_SCREEN         = 1 << 7;
        /// 2004 — bracketed paste.
        const BRACKETED_PASTE    = 1 << 8;
        /// 1000 — report button press and release.
        const MOUSE_CLICK        = 1 << 9;
        /// 1002 — also report motion while a button is held.
        const MOUSE_DRAG         = 1 << 10;
        /// 1003 — report all motion.
        const MOUSE_MOTION       = 1 << 11;
        /// 1006 — SGR mouse encoding. Without it, coordinates above 223 are
        /// unrepresentable, so any modern terminal wants this.
        const MOUSE_SGR          = 1 << 12;
        /// 1004 — report focus in/out.
        const FOCUS_REPORTING    = 1 << 13;
        /// 2026 — synchronized output.
        ///
        /// While set, frames are withheld so a full repaint appears atomically.
        /// Roughly thirty lines of work that removes tearing in neovim, tmux
        /// and every ratatui application.
        const SYNC_UPDATE        = 1 << 14;
        /// 9001 — win32 input mode, which ConPTY enables. Delivers key-up and
        /// modifier-only events to console programs.
        const WIN32_INPUT        = 1 << 15;
    }
}

impl Modes {
    /// The modes a terminal starts in.
    #[must_use]
    pub fn initial() -> Self {
        Modes::AUTO_WRAP | Modes::SHOW_CURSOR
    }

    /// True if any mouse reporting is active.
    #[must_use]
    pub fn mouse_enabled(self) -> bool {
        self.intersects(Modes::MOUSE_CLICK | Modes::MOUSE_DRAG | Modes::MOUSE_MOTION)
    }
}

/// Cursor shape, set by DECSCUSR (`CSI Ps SP q`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Cursor style: shape plus whether it blinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blinking: bool,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self { shape: CursorShape::Block, blinking: true }
    }
}

impl CursorStyle {
    /// Decode a DECSCUSR parameter.
    ///
    /// 0 and 1 are a blinking block, 2 steady block, 3 blinking underline,
    /// 4 steady underline, 5 blinking bar, 6 steady bar.
    #[must_use]
    pub fn from_decscusr(param: u16) -> Self {
        match param {
            0 | 1 => Self { shape: CursorShape::Block, blinking: true },
            2 => Self { shape: CursorShape::Block, blinking: false },
            3 => Self { shape: CursorShape::Underline, blinking: true },
            4 => Self { shape: CursorShape::Underline, blinking: false },
            5 => Self { shape: CursorShape::Bar, blinking: true },
            6 => Self { shape: CursorShape::Bar, blinking: false },
            _ => Self::default(),
        }
    }

    /// Encode back to a DECSCUSR parameter, for the wire.
    ///
    /// Never 0. Both 0 and 1 decode to a blinking block, so encoding as 1
    /// keeps `from_decscusr(to_decscusr(x)) == x` — 0 would round-trip in
    /// value but means "reset to the default", which is a different statement
    /// and stops being true the moment the default changes.
    #[must_use]
    pub fn to_decscusr(self) -> u16 {
        match (self.shape, self.blinking) {
            (CursorShape::Block, true) => 1,
            (CursorShape::Block, false) => 2,
            (CursorShape::Underline, true) => 3,
            (CursorShape::Underline, false) => 4,
            (CursorShape::Bar, true) => 5,
            (CursorShape::Bar, false) => 6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autowrap_and_cursor_start_on() {
        let m = Modes::initial();
        assert!(m.contains(Modes::AUTO_WRAP), "DECAWM defaults on");
        assert!(m.contains(Modes::SHOW_CURSOR));
        assert!(!m.contains(Modes::ALT_SCREEN));
        assert!(!m.mouse_enabled());
    }

    #[test]
    fn any_mouse_mode_counts_as_enabled() {
        assert!(Modes::MOUSE_CLICK.mouse_enabled());
        assert!(Modes::MOUSE_DRAG.mouse_enabled());
        assert!(Modes::MOUSE_MOTION.mouse_enabled());
        // SGR is an encoding, not a reporting mode -- on its own it reports nothing.
        assert!(!Modes::MOUSE_SGR.mouse_enabled());
    }

    #[test]
    fn decscusr_decodes_shape_and_blink() {
        assert_eq!(CursorStyle::from_decscusr(2).shape, CursorShape::Block);
        assert!(!CursorStyle::from_decscusr(2).blinking);
        assert_eq!(CursorStyle::from_decscusr(3).shape, CursorShape::Underline);
        assert!(CursorStyle::from_decscusr(3).blinking);
        assert_eq!(CursorStyle::from_decscusr(6).shape, CursorShape::Bar);
        assert!(!CursorStyle::from_decscusr(6).blinking);
        // 0 means "reset to default", which is a blinking block.
        assert_eq!(CursorStyle::from_decscusr(0), CursorStyle::default());
    }

    #[test]
    fn every_cursor_style_survives_the_wire() {
        // The remote protocol carries the DECSCUSR parameter, not the struct,
        // so a style that does not round-trip is one a remote client renders
        // differently from the machine the program is running on.
        for param in 1..=6u16 {
            let style = CursorStyle::from_decscusr(param);
            assert_eq!(
                style.to_decscusr(),
                param,
                "DECSCUSR {param} did not survive a round trip"
            );
        }
        for shape in [CursorShape::Block, CursorShape::Underline, CursorShape::Bar] {
            for blinking in [true, false] {
                let style = CursorStyle { shape, blinking };
                assert_eq!(CursorStyle::from_decscusr(style.to_decscusr()), style);
            }
        }
        assert_ne!(CursorStyle::default().to_decscusr(), 0, "0 means reset, not a style");
    }
}
