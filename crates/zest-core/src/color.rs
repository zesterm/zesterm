//! Terminal color as the escape stream expressed it.
//!
//! Deliberately *unresolved*: an SGR `38;5;12` is stored as `Indexed(12)`, not
//! as the RGB that index happens to mean under the current theme. Resolution
//! happens in the renderer, against a [`zest_theme::Theme`]. Two consequences
//! that are worth the discipline:
//!
//! - The same live session can be rendered under different themes by different
//!   clients at the same time (desktop dark, phone light).
//! - Changing the theme is a repaint, not a grid rewrite.

/// The 16 ANSI colors, by role rather than by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u8)]
pub enum NamedColor {
    Black = 0,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl NamedColor {
    /// The bright counterpart of a normal color; bright colors map to themselves.
    ///
    /// Used for the `bold-brightens-foreground` behavior that most terminals
    /// implement and some users switch off.
    #[must_use]
    pub const fn to_bright(self) -> Self {
        use NamedColor::*;
        match self {
            Black => BrightBlack,
            Red => BrightRed,
            Green => BrightGreen,
            Yellow => BrightYellow,
            Blue => BrightBlue,
            Magenta => BrightMagenta,
            Cyan => BrightCyan,
            White => BrightWhite,
            bright => bright,
        }
    }

    /// Construct from a 0..=15 palette index.
    #[must_use]
    pub const fn from_index(i: u8) -> Option<Self> {
        use NamedColor::*;
        Some(match i {
            0 => Black,
            1 => Red,
            2 => Green,
            3 => Yellow,
            4 => Blue,
            5 => Magenta,
            6 => Cyan,
            7 => White,
            8 => BrightBlack,
            9 => BrightRed,
            10 => BrightGreen,
            11 => BrightYellow,
            12 => BrightBlue,
            13 => BrightMagenta,
            14 => BrightCyan,
            15 => BrightWhite,
            _ => return None,
        })
    }
}

/// A cell's foreground or background color, unresolved.
///
/// Fits in 4 bytes so that [`crate::Cell`] can stay at 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
// No `ts(export)`: the binding is written into `crates/zest-proto/bindings/`
// as a dependency of `AttrDef`, which is the type that actually crosses the
// wire. An export test here would write a second copy under this crate.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Color {
    /// The theme's default foreground/background, depending on which slot this
    /// occupies. Kept distinct from `Indexed(7)`/`Indexed(0)` on purpose: a cell
    /// that never set a color must follow the theme when the theme changes.
    ///
    /// Also the marker that decides whether window opacity applies: only cells
    /// whose background is `Default` become translucent. Cells with an explicit
    /// background (dir colors, TUI panels) must stay opaque, or every TUI looks
    /// broken over a transparent window.
    #[default]
    Default,
    /// A palette index. 0..=15 are the named colors, 16..=231 the 6x6x6 cube,
    /// 232..=255 the grayscale ramp.
    Indexed(u8),
    /// A direct 24-bit color from `SGR 38;2;r;g;b`.
    Rgb(u8, u8, u8),
}

impl Color {
    /// True if this color follows the theme rather than pinning a value.
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self, Color::Default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_is_four_bytes() {
        // Cell is 16 bytes only if fg and bg are 4 each. If this fails, Cell
        // has silently grown and grid throughput will regress.
        assert_eq!(core::mem::size_of::<Color>(), 4);
    }

    #[test]
    fn bright_is_idempotent() {
        assert_eq!(NamedColor::Red.to_bright(), NamedColor::BrightRed);
        assert_eq!(NamedColor::BrightRed.to_bright(), NamedColor::BrightRed);
    }

    #[test]
    fn index_roundtrip() {
        for i in 0u8..16 {
            let c = NamedColor::from_index(i).expect("0..16 are all valid");
            assert_eq!(c as u8, i);
        }
        assert!(NamedColor::from_index(16).is_none());
    }
}
