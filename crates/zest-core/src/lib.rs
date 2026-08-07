//! Headless terminal emulation.
//!
//! This crate owns everything about *what the terminal contains* and nothing
//! about *how it looks*. It has no window, no GPU, no PTY, and no socket. It is
//! fed bytes and it maintains a grid.
//!
//! That boundary is the whole reason the project can grow a remote head later:
//! the native app, the daemon that serves browsers, and the mobile client all
//! consume this same crate rather than three divergent implementations.
//!
//! # Invariants
//!
//! - No dependency on `wgpu`, `winit`, `windows`, or `tokio`. Enforced in CI by
//!   `cargo xtask check-deps`.
//! - Builds for `wasm32-unknown-unknown` with `--no-default-features`.
//! - Colors are stored exactly as SGR delivered them and are **never** resolved
//!   to RGB here. Resolution against a theme is a rendering concern, which is
//!   what lets one session render under different themes on desktop and phone
//!   simultaneously.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cell;
pub mod color;
pub mod grid;
pub mod modes;
pub mod palette;
mod perform;
pub mod selection;
pub mod term;

pub use cell::{Cell, CellFlags};
pub use color::{Color, NamedColor};
pub use grid::{Cursor, Grid, LineId, Row, ScrollRegion};
pub use modes::{CursorShape, CursorStyle, Modes};
pub use palette::{Palette, PaletteSnapshot, Rgb};
pub use selection::{AbsPos, Selection, SelectionMode};
pub use term::{Damage, TermEvent, Terminal};
