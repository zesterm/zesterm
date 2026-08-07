//! The built-in themes.
//!
//! Names and values match `@sigx/terminal-zero`'s registered themes, so a
//! zesterm window and a `@sigx/terminal-ui` TUI running inside it agree on what
//! "obsidian" looks like.
//!
//! # Built through the public path, on purpose
//!
//! Each of these is assembled by [`theme_from_scheme`] — the same function every
//! importer ends with. Hand-writing all 24 tokens per theme would let the
//! built-ins drift into looking better than anything importable, which is the
//! classic failure mode: one gorgeous theme and a theming system that does not
//! really work.

use crate::resolve::theme_from_scheme;
use crate::tokens::{Rgba8, TerminalColors, Theme};

/// Every built-in, by id.
pub const IDS: &[&str] = &["obsidian", "nord", "gum", "classic", "paper"];

/// The default when nothing is configured.
pub const DEFAULT_DARK: &str = "obsidian";
pub const DEFAULT_LIGHT: &str = "paper";

const fn c(hex: u32) -> Rgba8 {
    Rgba8::rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Look up a built-in by id.
#[must_use]
pub fn get(id: &str) -> Option<Theme> {
    Some(match id {
        "obsidian" => obsidian(),
        "nord" => nord(),
        "gum" => gum(),
        "classic" => classic(),
        "paper" => paper(),
        _ => return None,
    })
}

/// All built-ins, for a theme picker.
#[must_use]
pub fn all() -> Vec<Theme> {
    IDS.iter().filter_map(|id| get(id)).collect()
}

/// sigx's default dark theme.
#[must_use]
pub fn obsidian() -> Theme {
    let normal = [
        c(0x0b0f1a), c(0xe0606a), c(0x5fd17f), c(0xe0b341),
        c(0x6ea8ff), c(0xb07cff), c(0x5fc4e0), c(0xd7dcea),
    ];
    build("obsidian", "Obsidian", c(0x0b0f1a), c(0xd7dcea), normal)
}

#[must_use]
pub fn nord() -> Theme {
    let normal = [
        c(0x3b4252), c(0xbf616a), c(0xa3be8c), c(0xebcb8b),
        c(0x81a1c1), c(0xb48ead), c(0x88c0d0), c(0xe5e9f0),
    ];
    build("nord", "Nord", c(0x2e3440), c(0xd8dee9), normal)
}

#[must_use]
pub fn gum() -> Theme {
    let normal = [
        c(0x2b2536), c(0xff5c8a), c(0x5ce6a8), c(0xffc857),
        c(0x7aa2ff), c(0xc792ea), c(0x5fd7e0), c(0xe6e1f0),
    ];
    build("gum", "Gum", c(0x1f1b2a), c(0xe6e1f0), normal)
}

/// The classic dark terminal palette — close to xterm's own.
#[must_use]
pub fn classic() -> Theme {
    let normal = [
        c(0x000000), c(0xcd0000), c(0x00cd00), c(0xcdcd00),
        c(0x0000ee), c(0xcd00cd), c(0x00cdcd), c(0xe5e5e5),
    ];
    build("classic", "Classic", c(0x000000), c(0xe5e5e5), normal)
}

/// The light theme.
#[must_use]
pub fn paper() -> Theme {
    let normal = [
        c(0x2b2b2b), c(0xc0392b), c(0x27803a), c(0x9a6b00),
        c(0x1f6feb), c(0x8250df), c(0x1b7c83), c(0x6e7781),
    ];
    build("paper", "Paper", c(0xfaf9f5), c(0x24292f), normal)
}

fn build(id: &str, name: &str, bg: Rgba8, fg: Rgba8, normal: [Rgba8; 8]) -> Theme {
    // Brights are derived rather than authored: eight more hand-picked hex
    // values per theme is eight more chances for a bright to drift out of the
    // theme's colour family.
    let bright = normal.map(crate::oklch::brighten_ansi);
    theme_from_scheme(id, name, bg, fg, normal, bright, TerminalColors::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oklch::contrast_ratio;
    use crate::resolve::resolve;
    use crate::tokens::ThemeMode;

    #[test]
    fn every_id_resolves() {
        for id in IDS {
            assert!(get(id).is_some(), "{id} is listed but missing");
        }
        assert_eq!(all().len(), IDS.len());
        assert!(get("no-such-theme").is_none());
    }

    #[test]
    fn ids_are_stable_and_match_the_struct() {
        for t in all() {
            assert!(IDS.contains(&t.id.as_str()), "{} is not in IDS", t.id);
            assert!(!t.name.is_empty());
        }
    }

    #[test]
    fn modes_are_detected_correctly() {
        assert_eq!(paper().mode, ThemeMode::Light);
        for id in ["obsidian", "nord", "gum", "classic"] {
            assert_eq!(get(id).unwrap().mode, ThemeMode::Dark, "{id}");
        }
    }

    #[test]
    fn every_theme_is_readable() {
        // WCAG AA for body text. A built-in failing this would be shipped-broken
        // in the most visible possible way.
        for t in all() {
            let ratio = contrast_ratio(t.ui.fg, t.ui.bg);
            assert!(ratio >= 4.5, "{} has only {ratio:.1}:1 foreground contrast", t.id);
        }
    }

    #[test]
    fn every_theme_has_visible_chrome() {
        for t in all() {
            assert_ne!(t.ui.panel, t.ui.bg, "{}: panel is invisible", t.id);
            assert!(
                contrast_ratio(t.ui.line, t.ui.bg) > 1.05,
                "{}: borders are invisible against the background",
                t.id
            );
        }
    }

    #[test]
    fn every_theme_resolves_to_a_full_palette() {
        for t in all() {
            let p = resolve(&t);
            assert_eq!(p.colors.len(), 256);
            assert!(
                p.colors.iter().all(|c| c.is_opaque()),
                "{}: palette entries must be opaque",
                t.id
            );
            assert!(
                contrast_ratio(p.cursor_text, p.cursor) > 3.0,
                "{}: text under the block cursor would be unreadable",
                t.id
            );
        }
    }

    #[test]
    fn obsidian_matches_the_sigx_values() {
        // The contract with @sigx/terminal-zero. If sigx changes its obsidian,
        // this fails and the two stop agreeing on what the theme looks like.
        let t = obsidian();
        assert_eq!(t.ui.bg, c(0x0b0f1a));
        assert_eq!(t.ui.fg, c(0xd7dcea));
        assert_eq!(t.ui.accent, c(0x6ea8ff));
        assert_eq!(t.ui.danger, c(0xe0606a));
        assert_eq!(t.ui.success, c(0x5fd17f));
        assert_eq!(t.ui.warn, c(0xe0b341));
        assert_eq!(t.ui.info, c(0x5fc4e0));
    }

    #[test]
    fn themes_round_trip_through_json() {
        for t in all() {
            let json = serde_json::to_string(&t).expect("serialize");
            let back: Theme = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, t, "{} did not survive a round trip", t.id);
        }
    }

    #[test]
    fn themes_round_trip_through_toml() {
        // Themes are authored in either format; both must be lossless.
        for t in all() {
            let text = toml::to_string(&t).expect("serialize");
            let back: Theme = toml::from_str(&text).expect("deserialize");
            assert_eq!(back, t, "{} did not survive a toml round trip", t.id);
        }
    }
}
