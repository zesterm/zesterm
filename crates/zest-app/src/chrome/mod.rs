//! Window chrome: the geometry, layout and hit testing around the grid.
//!
//! Everything here is pure and GPU-free on purpose — `check-deps` keeps
//! `zest-render-wgpu` from knowing about `winit`, so the layout half of the
//! chrome lives on the app side and hands the renderer finished instances.
//! The pure functions are what make the load-bearing tests possible: a hit
//! region and the rectangle it was drawn as come from the same computation,
//! so they cannot drift apart.

// The pure half lands ahead of the app wiring (next commit in the #23
// sequence), so until then only the tests reference it.
#[allow(dead_code, reason = "consumed by the redraw and input paths one commit later")]
pub mod blocks;
pub mod hit;
#[allow(dead_code, reason = "consumed by the redraw and input paths one commit later")]
pub mod screens;
pub mod insets;
#[allow(dead_code, reason = "consumed by the redraw and input paths one commit later")]
pub mod layout;
#[allow(dead_code, reason = "consumed by the redraw and input paths one commit later")]
pub mod model;
#[allow(dead_code, reason = "consumed by the redraw and input paths one commit later")]
pub mod theme;

pub use insets::Insets;
