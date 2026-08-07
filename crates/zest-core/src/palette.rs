//! The live color palette.
//!
//! # Why this lives in `zest-core` despite core being presentation-free
//!
//! Cells store colors *unresolved* ([`crate::Color`]), and resolving them
//! against a theme is a rendering concern. But escape sequences can both
//! **query** and **mutate** actual color values:
//!
//! - `OSC 4 ; n ; ?` — "what RGB is palette index n?"
//! - `OSC 10/11/12 ; ?` — "what are your fg / bg / cursor colors?"
//! - `OSC 4 ; 1 ; #ff0000` — "make index 1 red, for this session"
//! - `OSC 104 / 110 / 111 / 112` — "reset those back"
//!
//! Programs genuinely depend on this: many TUIs query the background to decide
//! whether they are on a light or dark terminal.
//!
//! So the division is: **`zest-theme` owns the *seed* palette; `zest-core` owns
//! the *live* one.** The app resolves a theme into a [`PaletteSnapshot`] and
//! seeds the terminal with it; escape sequences mutate the live copy; a reset
//! restores the seed; a theme change re-seeds.
//!
//! Terminals commonly get this wrong in one of two ways. Resolving colors at
//! parse time means a theme change requires rewriting every cell in scrollback.
//! Letting the theme own the mutable palette means `OSC 4` and a theme reload
//! fight each other. Keeping the seed and the live palette as separate values
//! avoids both.
//!
//! [`PaletteSnapshot`] is plain data with no theme semantics, which is what
//! keeps this from being a dependency on `zest-theme`.

/// A resolved 24-bit color. Distinct from [`crate::Color`], which is a
/// *reference* that may or may not name a concrete value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// The fully-resolved palette a session is currently using.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteSnapshot {
    /// Indices 0..=255. 0..=15 named, 16..=231 the 6x6x6 cube, 232..=255 gray.
    pub colors: [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
}

impl PaletteSnapshot {
    /// Fill indices 16..=255 with the standard xterm cube and grayscale ramp.
    ///
    /// These are defined by the spec rather than by taste, so they are computed
    /// rather than stored — a theme that wants different ones overrides them
    /// explicitly.
    pub fn fill_standard_extended(&mut self) {
        // 16..=231: a 6x6x6 RGB cube. The level ramp is not linear; these are
        // the values xterm uses and that every other terminal matches.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut i = 16usize;
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    self.colors[i] = Rgb::new(LEVELS[r], LEVELS[g], LEVELS[b]);
                    i += 1;
                }
            }
        }
        debug_assert_eq!(i, 232);

        // 232..=255: 24 steps of gray from 8 to 238, in increments of 10.
        for (n, idx) in (232usize..=255).enumerate() {
            let v = 8 + (n as u8) * 10;
            self.colors[idx] = Rgb::new(v, v, v);
        }
    }
}

/// Tracks the live palette against the seed so resets are exact.
#[derive(Debug, Clone)]
pub struct Palette {
    live: PaletteSnapshot,
    seed: PaletteSnapshot,
}

impl Palette {
    /// Seed the palette. Called at startup and whenever the theme changes.
    #[must_use]
    pub fn new(seed: PaletteSnapshot) -> Self {
        Self { live: seed.clone(), seed }
    }

    #[must_use]
    pub fn live(&self) -> &PaletteSnapshot {
        &self.live
    }

    /// Re-seed on a theme change.
    ///
    /// Deliberately discards live overrides: a theme change is an explicit user
    /// action and should produce the theme the user asked for, not a hybrid of
    /// it and whatever a program set an hour ago.
    pub fn reseed(&mut self, seed: PaletteSnapshot) {
        self.live = seed.clone();
        self.seed = seed;
    }

    /// `OSC 4 ; n ; #rrggbb`
    pub fn set_indexed(&mut self, index: u8, color: Rgb) {
        self.live.colors[index as usize] = color;
    }

    /// `OSC 104` with no argument resets every index; with arguments, only those.
    pub fn reset_indexed(&mut self, index: Option<u8>) {
        match index {
            Some(i) => self.live.colors[i as usize] = self.seed.colors[i as usize],
            None => self.live.colors = self.seed.colors,
        }
    }

    /// `OSC 10` / `OSC 110`
    pub fn set_foreground(&mut self, color: Rgb) {
        self.live.foreground = color;
    }
    pub fn reset_foreground(&mut self) {
        self.live.foreground = self.seed.foreground;
    }

    /// `OSC 11` / `OSC 111`
    pub fn set_background(&mut self, color: Rgb) {
        self.live.background = color;
    }
    pub fn reset_background(&mut self) {
        self.live.background = self.seed.background;
    }

    /// `OSC 12` / `OSC 112`
    pub fn set_cursor(&mut self, color: Rgb) {
        self.live.cursor = color;
    }
    pub fn reset_cursor(&mut self) {
        self.live.cursor = self.seed.cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> PaletteSnapshot {
        let mut p = PaletteSnapshot {
            colors: [Rgb::default(); 256],
            foreground: Rgb::new(0xd7, 0xdc, 0xea),
            background: Rgb::new(0x0b, 0x0f, 0x1a),
            cursor: Rgb::new(0x6e, 0xa8, 0xff),
        };
        p.fill_standard_extended();
        p
    }

    #[test]
    fn extended_palette_matches_xterm() {
        let p = snapshot();
        // First cube entry is pure black, last is pure white.
        assert_eq!(p.colors[16], Rgb::new(0, 0, 0));
        assert_eq!(p.colors[231], Rgb::new(255, 255, 255));
        // A known interior cube value. 33 - 16 = 17 = 36*0 + 6*2 + 5, so
        // r=0, g=2, b=5 -> #0087ff, which is what xterm reports for color 33.
        assert_eq!(p.colors[33], Rgb::new(0, 135, 255));
        // Grayscale ramp endpoints.
        assert_eq!(p.colors[232], Rgb::new(8, 8, 8));
        assert_eq!(p.colors[255], Rgb::new(238, 238, 238));
    }

    #[test]
    fn osc_set_then_reset_restores_seed() {
        let mut pal = Palette::new(snapshot());
        let original = pal.live().colors[1];

        pal.set_indexed(1, Rgb::new(0xff, 0, 0));
        assert_eq!(pal.live().colors[1], Rgb::new(0xff, 0, 0));

        pal.reset_indexed(Some(1));
        assert_eq!(pal.live().colors[1], original);
    }

    #[test]
    fn reset_all_leaves_other_state_alone() {
        let mut pal = Palette::new(snapshot());
        pal.set_indexed(1, Rgb::new(0xff, 0, 0));
        pal.set_background(Rgb::new(1, 2, 3));

        pal.reset_indexed(None);

        assert_eq!(pal.live().colors[1], pal.seed.colors[1]);
        // OSC 104 resets the indexed palette only -- not fg/bg/cursor.
        assert_eq!(pal.live().background, Rgb::new(1, 2, 3));
    }

    #[test]
    fn reseed_discards_live_overrides() {
        let mut pal = Palette::new(snapshot());
        pal.set_background(Rgb::new(1, 2, 3));

        let mut next = snapshot();
        next.background = Rgb::new(9, 9, 9);
        pal.reseed(next);

        assert_eq!(pal.live().background, Rgb::new(9, 9, 9));
        // ...and a subsequent reset lands on the NEW seed, not the old one.
        pal.set_background(Rgb::new(4, 4, 4));
        pal.reset_background();
        assert_eq!(pal.live().background, Rgb::new(9, 9, 9));
    }
}
