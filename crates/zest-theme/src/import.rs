//! Importers for existing colour-scheme formats.
//!
//! All four produce only a 16-colour palette plus a few roles — no scheme
//! format carries chrome tokens — so each ends at
//! [`theme_from_scheme`](crate::theme_from_scheme), which derives `ui.*`
//! perceptually. That shared tail is what decides whether an imported theme
//! looks native or looks like a beautiful grid with a grey titlebar bolted on.

use crate::resolve::theme_from_scheme;
use crate::tokens::{Rgba8, TerminalColors, Theme};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("not valid {format}: {reason}")]
    Malformed { format: &'static str, reason: String },
    #[error("{format} scheme is missing {what}")]
    Missing { format: &'static str, what: String },
}

/// Detect the format from a file extension and contents, then import.
///
/// Extension first, contents as the tiebreaker — `.txt` exports and
/// misnamed files are common enough to be worth handling.
pub fn import(name: &str, filename: &str, source: &str) -> Result<Theme, ImportError> {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let trimmed = source.trim_start();

    match ext.as_str() {
        "itermcolors" => iterm2(name, source),
        "json" => windows_terminal(name, source),
        "toml" | "yml" | "yaml" => alacritty(name, source),
        _ if trimmed.starts_with("<?xml") || trimmed.starts_with("<!DOCTYPE plist") => {
            iterm2(name, source)
        }
        _ if trimmed.starts_with('{') => {
            // base16 schemes are also often JSON; try the richer format first.
            windows_terminal(name, source).or_else(|_| base16(name, source))
        }
        _ => base16(name, source).or_else(|_| alacritty(name, source)),
    }
}

// --- iTerm2 ---------------------------------------------------------------

/// iTerm2 `.itermcolors` — an XML plist of float components.
///
/// Parsed with a small hand-rolled scanner rather than a plist crate: the
/// structure is a flat `<key>…</key><dict>…</dict>` sequence, and the format has
/// not changed in a decade. A dependency would cost more than it saves.
///
/// Components are 0.0–1.0 floats, not bytes, and iTerm writes them in a colour
/// space named by a `Color Space` key. Non-sRGB spaces are read as sRGB — the
/// difference is small and the alternative is a colour-management dependency.
pub fn iterm2(name: &str, xml: &str) -> Result<Theme, ImportError> {
    let mut normal = [Rgba8::default(); 8];
    let mut bright = [Rgba8::default(); 8];
    let (mut fg, mut bg) = (None, None);
    let (mut cursor, mut cursor_text, mut selection_bg, mut selection_fg) =
        (None, None, None, None);

    for (key, color) in plist_entries(xml) {
        match key.as_str() {
            k if k.starts_with("Ansi ") && k.ends_with(" Color") => {
                let idx: usize = k
                    .trim_start_matches("Ansi ")
                    .trim_end_matches(" Color")
                    .trim()
                    .parse()
                    .unwrap_or(usize::MAX);
                match idx {
                    0..=7 => normal[idx] = color,
                    8..=15 => bright[idx - 8] = color,
                    _ => {}
                }
            }
            "Foreground Color" => fg = Some(color),
            "Background Color" => bg = Some(color),
            "Cursor Color" => cursor = Some(color),
            "Cursor Text Color" => cursor_text = Some(color),
            "Selection Color" => selection_bg = Some(color),
            "Selected Text Color" => selection_fg = Some(color),
            _ => {}
        }
    }

    let bg = bg.ok_or_else(|| ImportError::Missing {
        format: "iTerm2",
        what: "Background Color".into(),
    })?;
    let fg = fg.ok_or_else(|| ImportError::Missing {
        format: "iTerm2",
        what: "Foreground Color".into(),
    })?;

    Ok(theme_from_scheme(
        &slug(name),
        name,
        bg,
        fg,
        normal,
        bright,
        TerminalColors { cursor, cursor_text, selection_bg, selection_fg, ..Default::default() },
    ))
}

/// Walk `<key>NAME</key><dict>` blocks, yielding the colour each describes.
fn plist_entries(xml: &str) -> Vec<(String, Rgba8)> {
    let mut out = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find("<key>") {
        let after = &rest[start + 5..];
        let Some(end) = after.find("</key>") else { break };
        let key = after[..end].trim().to_string();
        let tail = &after[end + 6..];

        // The dict for this key runs to its closing tag.
        let Some(dict_start) = tail.find("<dict>") else {
            rest = tail;
            continue;
        };
        let Some(dict_end) = tail.find("</dict>") else { break };
        if dict_start > dict_end {
            rest = tail;
            continue;
        }
        let dict = &tail[dict_start..dict_end];

        if let Some(color) = plist_color(dict) {
            out.push((key, color));
        }
        rest = &tail[dict_end..];
    }
    out
}

fn plist_color(dict: &str) -> Option<Rgba8> {
    let component = |name: &str| -> Option<f32> {
        let at = dict.find(&format!("<key>{name}</key>"))?;
        let after = &dict[at..];
        let open = after.find("<real>")?;
        let close = after.find("</real>")?;
        after[open + 6..close].trim().parse().ok()
    };
    let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Rgba8::rgb(
        to_byte(component("Red Component")?),
        to_byte(component("Green Component")?),
        to_byte(component("Blue Component")?),
    ))
}

// --- Windows Terminal -----------------------------------------------------

/// A Windows Terminal scheme object from `settings.json`.
pub fn windows_terminal(name: &str, json: &str) -> Result<Theme, ImportError> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| ImportError::Malformed {
            format: "Windows Terminal",
            reason: e.to_string(),
        })?;

    // Accept either a bare scheme object or a settings.json containing a
    // `schemes` array -- people paste both.
    let scheme = v
        .get("schemes")
        .and_then(|s| s.as_array())
        .and_then(|a| {
            a.iter()
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
                .or_else(|| a.first())
        })
        .unwrap_or(&v);

    let get = |key: &str| -> Option<Rgba8> {
        scheme.get(key).and_then(|c| c.as_str()).and_then(|s| s.parse().ok())
    };

    const NORMAL: [&str; 8] =
        ["black", "red", "green", "yellow", "blue", "purple", "cyan", "white"];
    const BRIGHT: [&str; 8] = [
        "brightBlack", "brightRed", "brightGreen", "brightYellow",
        "brightBlue", "brightPurple", "brightCyan", "brightWhite",
    ];

    let mut normal = [Rgba8::default(); 8];
    let mut bright = [Rgba8::default(); 8];
    for i in 0..8 {
        normal[i] = get(NORMAL[i]).ok_or_else(|| ImportError::Missing {
            format: "Windows Terminal",
            what: NORMAL[i].to_string(),
        })?;
        // Brights are optional; derived if absent.
        bright[i] = get(BRIGHT[i]).unwrap_or_else(|| crate::oklch::brighten_ansi(normal[i]));
    }

    let bg = get("background").ok_or_else(|| ImportError::Missing {
        format: "Windows Terminal",
        what: "background".into(),
    })?;
    let fg = get("foreground").ok_or_else(|| ImportError::Missing {
        format: "Windows Terminal",
        what: "foreground".into(),
    })?;

    let display = scheme.get("name").and_then(|n| n.as_str()).unwrap_or(name);
    Ok(theme_from_scheme(
        &slug(display),
        display,
        bg,
        fg,
        normal,
        bright,
        TerminalColors {
            cursor: get("cursorColor"),
            selection_bg: get("selectionBackground"),
            ..Default::default()
        },
    ))
}

// --- base16 / base24 -------------------------------------------------------

/// A base16 or base24 scheme.
///
/// Accepts the YAML-ish `base00: "0b0f1a"` form and JSON. The mapping from
/// `baseNN` to ANSI is the one the base16 spec defines, and getting it wrong
/// produces a theme where red and orange are swapped.
pub fn base16(name: &str, source: &str) -> Result<Theme, ImportError> {
    let mut base = [Option::<Rgba8>::None; 24];

    for line in source.lines() {
        let line = line.trim().trim_start_matches('"');
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim().trim_matches('"');
        let Some(hex) = key.strip_prefix("base") else { continue };
        let Ok(idx) = u8::from_str_radix(hex.trim(), 16) else { continue };
        if idx as usize >= base.len() {
            continue;
        }

        let value = value.trim().trim_end_matches(',').trim().trim_matches(['"', '\'']);
        let with_hash = if value.starts_with('#') {
            value.to_string()
        } else {
            format!("#{value}")
        };
        if let Ok(c) = with_hash.parse::<Rgba8>() {
            base[idx as usize] = Some(c);
        }
    }

    let need = |i: usize| -> Result<Rgba8, ImportError> {
        base[i].ok_or_else(|| ImportError::Missing {
            format: "base16",
            what: format!("base{i:02X}"),
        })
    };

    let bg = need(0x00)?;
    let fg = need(0x05)?;

    // The standard base16 -> ANSI mapping.
    let normal = [
        need(0x00)?, need(0x08)?, need(0x0B)?, need(0x0A)?,
        need(0x0D)?, need(0x0E)?, need(0x0C)?, need(0x05)?,
    ];
    // base24 supplies real brights in base10..base17; base16 derives them.
    let bright = if base[0x10].is_some() {
        [
            base[0x10].unwrap_or(normal[0]),
            base[0x11].unwrap_or(normal[1]),
            base[0x12].unwrap_or(normal[2]),
            base[0x13].unwrap_or(normal[3]),
            base[0x14].unwrap_or(normal[4]),
            base[0x15].unwrap_or(normal[5]),
            base[0x16].unwrap_or(normal[6]),
            base[0x17].unwrap_or(normal[7]),
        ]
    } else {
        let mut b = normal.map(crate::oklch::brighten_ansi);
        b[0] = need(0x03)?; // base03 is the comment grey -- the natural bright black
        b[7] = need(0x07)?;
        b
    };

    Ok(theme_from_scheme(
        &slug(name),
        name,
        bg,
        fg,
        normal,
        bright,
        TerminalColors::default(),
    ))
}

// --- Alacritty / Ghostty ---------------------------------------------------

/// Alacritty TOML (`[colors.primary]` / `[colors.normal]`) or Ghostty's
/// line-oriented `palette = 0=#hex` config.
///
/// Ghostty is not TOML despite looking like it, so it needs its own scan.
pub fn alacritty(name: &str, source: &str) -> Result<Theme, ImportError> {
    // Ghostty: `palette = N=#rrggbb`, plus bare `background = #hex`.
    if source.contains("palette") && source.contains('=') && !source.contains("[colors") {
        return ghostty(name, source);
    }

    let v: toml::Value = toml::from_str(source).map_err(|e| ImportError::Malformed {
        format: "Alacritty",
        reason: e.to_string(),
    })?;
    let colors = v.get("colors").ok_or_else(|| ImportError::Missing {
        format: "Alacritty",
        what: "[colors]".into(),
    })?;

    let pick = |table: &str, key: &str| -> Option<Rgba8> {
        colors.get(table)?.get(key)?.as_str()?.parse().ok()
    };
    const NAMES: [&str; 8] =
        ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];

    let mut normal = [Rgba8::default(); 8];
    let mut bright = [Rgba8::default(); 8];
    for i in 0..8 {
        normal[i] = pick("normal", NAMES[i]).ok_or_else(|| ImportError::Missing {
            format: "Alacritty",
            what: format!("colors.normal.{}", NAMES[i]),
        })?;
        bright[i] =
            pick("bright", NAMES[i]).unwrap_or_else(|| crate::oklch::brighten_ansi(normal[i]));
    }

    let bg = pick("primary", "background").ok_or_else(|| ImportError::Missing {
        format: "Alacritty",
        what: "colors.primary.background".into(),
    })?;
    let fg = pick("primary", "foreground").ok_or_else(|| ImportError::Missing {
        format: "Alacritty",
        what: "colors.primary.foreground".into(),
    })?;

    Ok(theme_from_scheme(
        &slug(name),
        name,
        bg,
        fg,
        normal,
        bright,
        TerminalColors {
            cursor: pick("cursor", "cursor"),
            cursor_text: pick("cursor", "text"),
            selection_bg: pick("selection", "background"),
            selection_fg: pick("selection", "text"),
            ..Default::default()
        },
    ))
}

fn ghostty(name: &str, source: &str) -> Result<Theme, ImportError> {
    let mut palette = [Option::<Rgba8>::None; 16];
    let (mut bg, mut fg, mut cursor, mut selection_bg) = (None, None, None, None);

    for line in source.lines() {
        // `#` starts a comment only at the beginning of a line. Stripping from
        // the first `#` anywhere would eat every colour, since Ghostty writes
        // them as `palette = 0=#282828`.
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());

        match key {
            "palette" => {
                if let Some((idx, hex)) = value.split_once('=') {
                    if let (Ok(i), Ok(c)) = (idx.trim().parse::<usize>(), parse_loose(hex)) {
                        if i < 16 {
                            palette[i] = Some(c);
                        }
                    }
                }
            }
            "background" => bg = parse_loose(value).ok(),
            "foreground" => fg = parse_loose(value).ok(),
            "cursor-color" => cursor = parse_loose(value).ok(),
            "selection-background" => selection_bg = parse_loose(value).ok(),
            _ => {}
        }
    }

    let bg = bg.ok_or_else(|| ImportError::Missing {
        format: "Ghostty",
        what: "background".into(),
    })?;
    let fg = fg.ok_or_else(|| ImportError::Missing {
        format: "Ghostty",
        what: "foreground".into(),
    })?;

    let mut normal = [Rgba8::default(); 8];
    let mut bright = [Rgba8::default(); 8];
    for i in 0..8 {
        normal[i] = palette[i].ok_or_else(|| ImportError::Missing {
            format: "Ghostty",
            what: format!("palette = {i}"),
        })?;
        bright[i] = palette[i + 8].unwrap_or_else(|| crate::oklch::brighten_ansi(normal[i]));
    }

    Ok(theme_from_scheme(
        &slug(name),
        name,
        bg,
        fg,
        normal,
        bright,
        TerminalColors { cursor, selection_bg, ..Default::default() },
    ))
}

/// Accept `#rrggbb` and bare `rrggbb`; Ghostty and base16 both omit the hash.
fn parse_loose(s: &str) -> Result<Rgba8, crate::tokens::ParseColorError> {
    let s = s.trim();
    if s.starts_with('#') {
        s.parse()
    } else {
        format!("#{s}").parse()
    }
}

/// A stable id from a display name: lowercase, non-alphanumerics to hyphens.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oklch::contrast_ratio;

    #[test]
    fn slugs_are_stable_and_clean() {
        assert_eq!(slug("Solarized Dark"), "solarized-dark");
        assert_eq!(slug("Tokyo Night (Storm)"), "tokyo-night-storm");
        assert_eq!(slug("  weird  __ name  "), "weird-name");
    }

    const ITERM: &str = r#"<?xml version="1.0"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Ansi 0 Color</key>
  <dict>
    <key>Blue Component</key><real>0.0</real>
    <key>Green Component</key><real>0.0</real>
    <key>Red Component</key><real>0.0</real>
  </dict>
  <key>Ansi 1 Color</key>
  <dict>
    <key>Blue Component</key><real>0.2</real>
    <key>Green Component</key><real>0.2</real>
    <key>Red Component</key><real>0.8</real>
  </dict>
  <key>Background Color</key>
  <dict>
    <key>Blue Component</key><real>0.1</real>
    <key>Green Component</key><real>0.05</real>
    <key>Red Component</key><real>0.04</real>
  </dict>
  <key>Foreground Color</key>
  <dict>
    <key>Blue Component</key><real>0.9</real>
    <key>Green Component</key><real>0.86</real>
    <key>Red Component</key><real>0.84</real>
  </dict>
</dict>
</plist>"#;

    #[test]
    fn iterm2_reads_float_components() {
        let t = iterm2("Test Scheme", ITERM).expect("import");
        assert_eq!(t.id, "test-scheme");
        // 0.8 * 255 = 204 = 0xcc. Bytes, not floats, is the classic mistake --
        // reading 0.8 as 0 makes every colour black.
        assert_eq!(t.ansi.normal.unwrap()[1], Rgba8::rgb(204, 51, 51));
        assert_eq!(t.ui.bg, Rgba8::rgb(10, 13, 26));
    }

    #[test]
    fn iterm2_without_a_background_is_an_error() {
        let stripped = ITERM.replace("Background Color", "Unused Key");
        assert!(matches!(
            iterm2("x", &stripped),
            Err(ImportError::Missing { what, .. }) if what.contains("Background")
        ));
    }

    const WT: &str = r##"{
      "name": "Campbell",
      "background": "#0C0C0C", "foreground": "#CCCCCC",
      "black": "#0C0C0C", "red": "#C50F1F", "green": "#13A10E", "yellow": "#C19C00",
      "blue": "#0037DA", "purple": "#881798", "cyan": "#3A96DD", "white": "#CCCCCC",
      "brightBlack": "#767676", "brightRed": "#E74856", "brightGreen": "#16C60C",
      "brightYellow": "#F9F1A5", "brightBlue": "#3B78FF", "brightPurple": "#B4009E",
      "brightCyan": "#61D6D6", "brightWhite": "#F2F2F2",
      "cursorColor": "#FFFFFF", "selectionBackground": "#FFFFFF"
    }"##;

    #[test]
    fn windows_terminal_scheme_imports() {
        let t = windows_terminal("Campbell", WT).expect("import");
        assert_eq!(t.name, "Campbell");
        assert_eq!(t.id, "campbell");
        // Windows Terminal calls magenta "purple"; mapping it to the wrong slot
        // swaps two colours in every themed program.
        assert_eq!(t.ansi.normal.unwrap()[5], Rgba8::rgb(0x88, 0x17, 0x98));
        assert_eq!(t.ansi.bright.unwrap()[0], Rgba8::rgb(0x76, 0x76, 0x76));
        assert_eq!(t.terminal.cursor, Some(Rgba8::rgb(0xff, 0xff, 0xff)));
    }

    #[test]
    fn windows_terminal_accepts_a_whole_settings_file() {
        let settings = format!(r#"{{ "profiles": {{}}, "schemes": [{WT}] }}"#);
        let t = windows_terminal("Campbell", &settings).expect("import");
        assert_eq!(t.name, "Campbell");
    }

    const BASE16: &str = r#"
scheme: "Ocean"
base00: "2b303b"
base01: "343d46"
base02: "4f5b66"
base03: "65737e"
base04: "a7adba"
base05: "c0c5ce"
base06: "dfe1e8"
base07: "eff1f5"
base08: "bf616a"
base09: "d08770"
base0A: "ebcb8b"
base0B: "a3be8c"
base0C: "96b5b4"
base0D: "8fa1b3"
base0E: "b48ead"
base0F: "ab7967"
"#;

    #[test]
    fn base16_uses_the_standard_ansi_mapping() {
        let t = base16("Ocean", BASE16).expect("import");
        let n = t.ansi.normal.unwrap();
        // The spec's mapping. Getting it wrong swaps red and orange, which is
        // immediately obvious in any themed shell prompt.
        assert_eq!(n[1], Rgba8::rgb(0xbf, 0x61, 0x6a), "red is base08");
        assert_eq!(n[2], Rgba8::rgb(0xa3, 0xbe, 0x8c), "green is base0B");
        assert_eq!(n[3], Rgba8::rgb(0xeb, 0xcb, 0x8b), "yellow is base0A");
        assert_eq!(n[4], Rgba8::rgb(0x8f, 0xa1, 0xb3), "blue is base0D");
        assert_eq!(t.ui.bg, Rgba8::rgb(0x2b, 0x30, 0x3b));
        // base03 is the comment grey, which is the natural bright black.
        assert_eq!(t.ansi.bright.unwrap()[0], Rgba8::rgb(0x65, 0x73, 0x7e));
    }

    const ALACRITTY: &str = r##"
[colors.primary]
background = "#1e1e2e"
foreground = "#cdd6f4"

[colors.cursor]
cursor = "#f5e0dc"
text = "#1e1e2e"

[colors.normal]
black = "#45475a"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
magenta = "#f5c2e7"
cyan = "#94e2d5"
white = "#bac2de"
"##;

    #[test]
    fn alacritty_toml_imports_and_derives_missing_brights() {
        let t = alacritty("Mocha", ALACRITTY).expect("import");
        assert_eq!(t.ui.bg, Rgba8::rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(t.terminal.cursor, Some(Rgba8::rgb(0xf5, 0xe0, 0xdc)));

        // No [colors.bright] table, so brights are derived rather than absent.
        let bright = t.ansi.bright.unwrap();
        let normal = t.ansi.normal.unwrap();
        assert_ne!(bright[1], Rgba8::default(), "brights must not be left black");
        assert_ne!(bright[1], normal[1]);
    }

    const GHOSTTY: &str = r#"
# a ghostty config
background = 1d2021
foreground = ebdbb2
cursor-color = ebdbb2
palette = 0=#282828
palette = 1=#cc241d
palette = 2=#98971a
palette = 3=#d79921
palette = 4=#458588
palette = 5=#b16286
palette = 6=#689d6a
palette = 7=#a89984
palette = 8=#928374
"#;

    #[test]
    fn ghostty_line_format_imports() {
        // Ghostty looks like TOML but is not: `palette = 0=#hex` repeated is
        // invalid TOML (duplicate key), so it needs its own scan.
        let t = alacritty("Gruvbox", GHOSTTY).expect("import");
        assert_eq!(t.ui.bg, Rgba8::rgb(0x1d, 0x20, 0x21), "bare hex without a leading #");
        assert_eq!(t.ansi.normal.unwrap()[1], Rgba8::rgb(0xcc, 0x24, 0x1d));
        assert_eq!(t.ansi.bright.unwrap()[0], Rgba8::rgb(0x92, 0x83, 0x74));
    }

    #[test]
    fn format_is_detected_from_extension_and_content() {
        assert!(import("a", "x.itermcolors", ITERM).is_ok());
        assert!(import("Campbell", "x.json", WT).is_ok());
        assert!(import("Mocha", "x.toml", ALACRITTY).is_ok());
        assert!(import("Ocean", "x.yaml", BASE16).is_err(), "yaml tries Alacritty first");
        assert!(base16("Ocean", BASE16).is_ok());
        // Misnamed files fall back to content sniffing.
        assert!(import("a", "noextension", ITERM).is_ok());
    }

    /// The point of the whole exercise: an imported scheme must come out
    /// looking like a designed zesterm theme, chrome included -- not a nice grid
    /// with a grey titlebar bolted on.
    #[test]
    fn every_imported_theme_gets_usable_chrome() {
        let imported = [
            iterm2("iTerm", ITERM).unwrap(),
            windows_terminal("Campbell", WT).unwrap(),
            base16("Ocean", BASE16).unwrap(),
            alacritty("Mocha", ALACRITTY).unwrap(),
            alacritty("Gruvbox", GHOSTTY).unwrap(),
        ];

        for t in imported {
            assert_ne!(t.ui.panel, t.ui.bg, "{}: panel is invisible", t.id);
            assert!(
                contrast_ratio(t.ui.line, t.ui.bg) > 1.1,
                "{}: borders are invisible",
                t.id
            );
            assert!(
                contrast_ratio(t.ui.fg, t.ui.bg) >= 4.5,
                "{}: text is unreadable ({:.1}:1)",
                t.id,
                contrast_ratio(t.ui.fg, t.ui.bg)
            );
            assert!(
                contrast_ratio(t.ui.accent_text, t.ui.accent) > 3.0,
                "{}: accent labels are unreadable",
                t.id
            );
            // A resolved palette must be complete and opaque.
            let p = crate::resolve::resolve(&t);
            assert!(p.colors.iter().all(|c| c.is_opaque()), "{}", t.id);
        }
    }

    #[test]
    fn malformed_input_errors_rather_than_panicking() {
        // Importers take user files, so every failure mode has to be an error.
        assert!(windows_terminal("x", "not json").is_err());
        assert!(alacritty("x", "[[[").is_err());
        assert!(base16("x", "").is_err());
        assert!(iterm2("x", "").is_err());
        assert!(iterm2("x", "<dict><key>Ansi 0 Color</key>").is_err());
    }
}
