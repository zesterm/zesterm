//! Theme tokens, color math, and importers for existing scheme formats.
//!
//! The `ui` block is [`@sigx/terminal-zero`]'s token contract *verbatim*, so a
//! single theme file styles zesterm's own chrome and any `@sigx/terminal-ui`
//! TUI running inside it. `ansi` overrides the terminal palette; `terminal`
//! carries roles a TUI framework has no concept of.
//!
//! # Constraints
//!
//! This crate has **no filesystem access and no OS dependencies** — every entry
//! point takes `&str`. That is deliberate: it must compile to
//! `wasm32-unknown-unknown` so the SignalX web and Lynx clients run *this*
//! [`resolve`] rather than a hand-ported JavaScript copy that drifts.
//!
//! [`@sigx/terminal-zero`]: https://sigx.dev/terminal/docs/theming

#![forbid(unsafe_code)]

pub mod builtin;
pub mod derived;
pub mod import;
pub mod oklch;
pub mod resolve;
pub mod tokens;

pub use resolve::{derive_ui_tokens, resolve, theme_from_scheme, ResolvedPalette};
pub use tokens::{
    AnsiPalette, ParseColorError, Rgba8, TerminalColors, Theme, ThemeEffects, ThemeMode, UiTokens,
    CURRENT_SCHEMA,
};
