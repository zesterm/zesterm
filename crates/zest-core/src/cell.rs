//! The grid cell.
//!
//! # Why 16 bytes, exactly
//!
//! A 300x100 viewport plus 10k lines of scrollback is on the order of three
//! million cells. At 16 bytes that is ~48 MB and, more importantly, four cells
//! per cache line while scanning a row. Every byte added costs throughput on
//! the hottest loop in the program.
//!
//! The specific mistake to avoid is storing a `String` per cell to handle
//! combining marks. It turns a 16-byte value into 40 bytes plus a heap
//! allocation and destroys locality, for a case that occurs in well under one
//! cell in a thousand. Those go in a side table indexed by [`Cell::extra`].

use crate::Color;

bitflags::bitflags! {
    /// Per-cell rendering attributes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct CellFlags: u16 {
        const BOLD              = 1 << 0;
        const DIM               = 1 << 1;
        const ITALIC            = 1 << 2;
        const UNDERLINE         = 1 << 3;
        const DOUBLE_UNDERLINE  = 1 << 4;
        const UNDERCURL         = 1 << 5;
        const STRIKEOUT         = 1 << 6;
        const INVERSE           = 1 << 7;
        const HIDDEN            = 1 << 8;
        const BLINK             = 1 << 9;

        /// This cell holds a double-width character (CJK, most emoji).
        const WIDE              = 1 << 10;
        /// The right half of a double-width character. Holds no glyph of its
        /// own; the renderer skips it, and cursor movement and selection must
        /// treat the pair atomically.
        const WIDE_SPACER       = 1 << 11;

        /// Set on the last cell of a row that wrapped rather than ended.
        ///
        /// Load-bearing for two later features: copying a wrapped line must not
        /// insert a newline, and reflow-on-resize (M3) needs to know which rows
        /// may be rejoined. Two bits now, no data migration later.
        const WRAPLINE          = 1 << 12;
    }
}

/// Index into a row's side table for the rare cell that needs more than a
/// single `char`. [`Cell::extra`] is this, with `NONE` meaning "nothing extra".
pub type ExtraId = u16;

/// No side-table entry.
pub const NO_EXTRA: ExtraId = u16::MAX;

/// A single grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Cell {
    /// The base character. Combining marks live in the side table.
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    /// Side-table index for combining marks and OSC 8 hyperlink ids.
    /// [`NO_EXTRA`] for the overwhelming majority of cells.
    ///
    /// **Command block ids are deliberately not here**, though an earlier note
    /// said they would be. A block spans thousands of lines, so a per-cell id
    /// would be both enormous and wrong the moment a line is evicted while the
    /// block is still running — see [`crate::blocks`]. It would also be
    /// invisible to every remote client: `extra` was an excluded field in the
    /// protocol conformance comparator until combining marks were given their
    /// own wire representation, precisely because the encoder does not read it.
    pub extra: ExtraId,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: CellFlags::empty(),
            extra: NO_EXTRA,
        }
    }
}

impl Cell {
    /// A blank cell carrying `template`'s colors.
    ///
    /// Erase operations paint the *current* SGR background rather than the
    /// theme default -- that is what makes `ED`/`EL` inside a colored region
    /// leave the right color behind.
    #[must_use]
    pub fn blank_with(template: &Cell) -> Self {
        Self {
            ch: ' ',
            fg: template.fg,
            bg: template.bg,
            // Only the background-ish attributes survive an erase. Carrying
            // BOLD or UNDERLINE into erased space would underline empty cells.
            flags: template.flags & CellFlags::INVERSE,
            extra: NO_EXTRA,
        }
    }

    /// True if nothing would be painted for this cell but its background.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        (self.ch == ' ' || self.ch == '\0')
            && self.extra == NO_EXTRA
            && !self.flags.intersects(
                CellFlags::UNDERLINE
                    | CellFlags::DOUBLE_UNDERLINE
                    | CellFlags::UNDERCURL
                    | CellFlags::STRIKEOUT,
            )
    }

    /// Reset to blank while keeping the allocation-free `Copy` semantics.
    pub fn reset(&mut self, template: &Cell) {
        *self = Cell::blank_with(template);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_is_sixteen_bytes() {
        // If this fails, grid throughput has silently regressed and the cache
        // behavior of every row scan changed. Fix the layout, don't bump the
        // number.
        assert_eq!(core::mem::size_of::<Cell>(), 16);
    }

    #[test]
    fn erase_keeps_background_not_decoration() {
        let template = Cell {
            bg: Color::Indexed(4),
            flags: CellFlags::UNDERLINE | CellFlags::BOLD | CellFlags::INVERSE,
            ..Default::default()
        };

        let blank = Cell::blank_with(&template);
        assert_eq!(blank.bg, Color::Indexed(4), "erased space keeps the SGR background");
        assert!(!blank.flags.contains(CellFlags::UNDERLINE), "erased space is not underlined");
        assert!(!blank.flags.contains(CellFlags::BOLD));
        assert!(blank.flags.contains(CellFlags::INVERSE), "inverse still affects the background");
    }

    #[test]
    fn blankness_accounts_for_decoration() {
        assert!(Cell::default().is_blank());

        let underlined = Cell { flags: CellFlags::UNDERLINE, ..Default::default() };
        assert!(!underlined.is_blank(), "an underlined space still paints something");

        let bolded = Cell { flags: CellFlags::BOLD, ..Default::default() };
        assert!(bolded.is_blank(), "bold on a space paints nothing");
    }
}
