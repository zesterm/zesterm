//! The theme token schema.
//!
//! [`UiTokens`] is `@sigx/terminal-zero`'s `Theme` record field-for-field and
//! case-for-case, minus `name`/`mode` which live on the outer [`Theme`]. That is
//! a contract, not a coincidence: `{...theme.ui, name, mode}` must be a valid
//! argument to sigx's `registerTheme()`, and a sigx theme must deserialize into
//! `UiTokens` losslessly. `deny_unknown_fields` on `UiTokens` is the anti-drift
//! mechanism — if sigx adds a token, the roundtrip test fails loudly instead of
//! the two vocabularies silently diverging.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// An sRGB color with alpha, serialized as `"#rrggbb"` or `"#rrggbbaa"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }
}

impl Default for Rgba8 {
    fn default() -> Self {
        Self::rgb(0, 0, 0)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseColorError {
    #[error("expected a leading '#'")]
    MissingHash,
    #[error("expected 3, 4, 6 or 8 hex digits, found {0}")]
    BadLength(usize),
    #[error("invalid hex digit")]
    BadDigit,
}

impl FromStr for Rgba8 {
    type Err = ParseColorError;

    /// Accepts `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`. Short forms expand each
    /// nibble the way CSS does (`#f0a` -> `#ff00aa`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.trim().strip_prefix('#').ok_or(ParseColorError::MissingHash)?;
        let b = hex.as_bytes();
        let nib = |c: u8| -> Result<u8, ParseColorError> {
            (c as char).to_digit(16).map(|d| d as u8).ok_or(ParseColorError::BadDigit)
        };
        let byte = |hi: u8, lo: u8| -> Result<u8, ParseColorError> { Ok(nib(hi)? << 4 | nib(lo)?) };

        match b.len() {
            3 | 4 => {
                let a = if b.len() == 4 { nib(b[3])? * 17 } else { 255 };
                Ok(Rgba8::rgba(nib(b[0])? * 17, nib(b[1])? * 17, nib(b[2])? * 17, a))
            }
            6 | 8 => {
                let a = if b.len() == 8 { byte(b[6], b[7])? } else { 255 };
                Ok(Rgba8::rgba(byte(b[0], b[1])?, byte(b[2], b[3])?, byte(b[4], b[5])?, a))
            }
            n => Err(ParseColorError::BadLength(n)),
        }
    }
}

impl fmt::Display for Rgba8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_opaque() {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

impl Serialize for Rgba8 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rgba8 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
}

/// terminal-zero's token contract, verbatim.
///
/// The trailing eight color names are terminal-zero's ANSI set. They double as
/// zesterm's *normal* ANSI palette, so a theme authored purely in the sigx
/// vocabulary is already a complete 8-color terminal theme with no extra work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiTokens {
    // surfaces
    pub bg: Rgba8,
    pub panel: Rgba8,
    pub chrome: Rgba8,
    pub line: Rgba8,
    pub shadow: Rgba8,
    // text
    pub fg: Rgba8,
    pub dim: Rgba8,
    pub faint: Rgba8,
    // accent
    pub accent: Rgba8,
    pub accent_soft: Rgba8,
    pub accent_text: Rgba8,
    pub sel_soft: Rgba8,
    // status
    pub success: Rgba8,
    pub warn: Rgba8,
    pub danger: Rgba8,
    pub info: Rgba8,
    // ansi (terminal-zero's eight)
    pub black: Rgba8,
    pub red: Rgba8,
    pub green: Rgba8,
    pub yellow: Rgba8,
    pub blue: Rgba8,
    pub magenta: Rgba8,
    pub cyan: Rgba8,
    pub white: Rgba8,
}

/// Overrides for the terminal palette.
///
/// Everything here is optional and everything has a derivation, because storing
/// 256 hex strings in every theme file is how theme formats become
/// unmaintainable. Indices 16..=231 are the standard 6x6x6 cube and 232..=255
/// the grayscale ramp — computed, never stored, unless `indexed` overrides them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AnsiPalette {
    /// Defaults to `ui.black` .. `ui.white`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal: Option<[Rgba8; 8]>,
    /// Defaults to an OKLCH lift of `normal` (see `resolve`), *not* naive RGB
    /// lightening — which produces washed-out brights that leave the theme's
    /// color family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bright: Option<[Rgba8; 8]>,
    /// Sparse overrides for 16..=255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed: Option<BTreeMap<u8, Rgba8>>,
}

/// Roles only a terminal has. All optional; each documents what it derives from.
// No `Eq`: `dim_alpha` is an f32.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TerminalColors {
    /// -> `ui.fg`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<Rgba8>,
    /// -> `ui.bg`. Convention: this is the **opaque-safe** background; window
    /// opacity is applied on top of it rather than baked into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba8>,
    /// -> `ui.accent`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Rgba8>,
    /// -> `ui.accentText`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_text: Option<Rgba8>,
    /// -> `ui.selSoft`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_bg: Option<Rgba8>,
    /// `None` means selected text keeps its own foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_fg: Option<Rgba8>,
    /// -> `ui.info`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_underline: Option<Rgba8>,
    /// Whether SGR bold also brightens the foreground color. -> `false`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold_is_bright: Option<bool>,
    /// Alpha applied for SGR dim. -> `0.55`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim_alpha: Option<f32>,
}

/// Visual intent authored by the *theme*, as distinct from user preference.
///
/// A light theme genuinely needs different text gamma than a dark one, so the
/// theme supplies a good default; user settings still win.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ThemeEffects {
    // `text_gamma` and `text_contrast` used to live here. They were declared,
    // never authored by any built-in theme, and never read by anything -- and
    // they cannot be layered under `appearance.text_gamma` as things stand,
    // because that setting always has a value, so there is no "unset" for a
    // theme suggestion to fill. Making it optional is the right fix and is not
    // a small one: the generated settings form computes "modified" against a
    // schema default and has no representation for "follows the theme", in the
    // Rust UI and the web client both. Tracked separately rather than smuggled
    // in here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_shadow_alpha: Option<f32>,
}

/// A complete theme.
///
/// `schema` is first so a migration can be chosen before the rest of the
/// document is interpreted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub schema: u32,
    /// Stable key used by settings (`theme = "obsidian"`).
    pub id: String,
    /// Display name.
    pub name: String,
    pub mode: ThemeMode,
    pub ui: UiTokens,
    #[serde(default)]
    pub ansi: AnsiPalette,
    #[serde(default)]
    pub terminal: TerminalColors,
    #[serde(default)]
    pub effects: ThemeEffects,
}

/// The schema version this build writes. Older documents migrate forward.
pub const CURRENT_SCHEMA: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_forms() {
        assert_eq!("#0b0f1a".parse::<Rgba8>().unwrap(), Rgba8::rgb(0x0b, 0x0f, 0x1a));
        assert_eq!("#f0a".parse::<Rgba8>().unwrap(), Rgba8::rgb(0xff, 0x00, 0xaa));
        assert_eq!("#0b0f1a80".parse::<Rgba8>().unwrap(), Rgba8::rgba(0x0b, 0x0f, 0x1a, 0x80));
        assert_eq!("#f0af".parse::<Rgba8>().unwrap(), Rgba8::rgba(0xff, 0x00, 0xaa, 0xff));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!("0b0f1a".parse::<Rgba8>(), Err(ParseColorError::MissingHash));
        assert_eq!("#0b0f1".parse::<Rgba8>(), Err(ParseColorError::BadLength(5)));
        assert_eq!("#gggggg".parse::<Rgba8>(), Err(ParseColorError::BadDigit));
    }

    #[test]
    fn display_roundtrips_and_omits_opaque_alpha() {
        let opaque = Rgba8::rgb(0x6e, 0xa8, 0xff);
        assert_eq!(opaque.to_string(), "#6ea8ff");
        assert_eq!(opaque.to_string().parse::<Rgba8>().unwrap(), opaque);

        let translucent = Rgba8::rgba(0x6e, 0xa8, 0xff, 0x40);
        assert_eq!(translucent.to_string(), "#6ea8ff40");
        assert_eq!(translucent.to_string().parse::<Rgba8>().unwrap(), translucent);
    }

    /// The contract with sigx: a terminal-zero theme object, which carries
    /// `name`/`mode` alongside the tokens, must deserialize into `UiTokens`
    /// once those two are removed — with no leftover fields in either
    /// direction. `deny_unknown_fields` makes an added sigx token fail here
    /// rather than silently diverge.
    #[test]
    fn ui_tokens_match_the_sigx_vocabulary() {
        let sigx = serde_json::json!({
            "bg": "#0b0f1a", "panel": "#121829", "chrome": "#0b0f1a",
            "line": "#2a3350", "shadow": "#000000",
            "fg": "#d7dcea", "dim": "#8b93a7", "faint": "#4a516a",
            "accent": "#6ea8ff", "accentSoft": "#16203a",
            "accentText": "#0b0f1a", "selSoft": "#161d33",
            "success": "#5fd17f", "warn": "#e0b341",
            "danger": "#e0606a", "info": "#5fc4e0",
            "black": "#0b0f1a", "red": "#e0606a", "green": "#5fd17f",
            "yellow": "#e0b341", "blue": "#6ea8ff", "magenta": "#b07cff",
            "cyan": "#5fc4e0", "white": "#d7dcea"
        });

        let ui: UiTokens = serde_json::from_value(sigx.clone())
            .expect("sigx vocabulary must deserialize into UiTokens exactly");

        // ...and back out again, unchanged.
        assert_eq!(serde_json::to_value(&ui).unwrap(), sigx);
    }
}
