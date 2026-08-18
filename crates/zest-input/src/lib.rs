//! Turning keyboard and mouse events into terminal byte sequences.
//!
//! A crate rather than a module in `zest-app` because two things now consume
//! it: the window driving a shell in this process, and the same window driving a
//! shell on another machine. In the fleet, encoding happens **at the keyboard** —
//! the protocol carries already-encoded bytes — because modifier and keymap
//! conventions belong to the platform that produced the keystroke, not to the
//! platform the shell happens to run on.
//!
//! # The thing that is easy to get backwards
//!
//! **Modes change the encoding.** Arrow keys are `ESC [ A` normally and
//! `ESC O A` under DECCKM. Getting that wrong breaks vim and readline in a way
//! that reads as the terminal randomly ignoring the keyboard, which sends people
//! looking at the wrong layer entirely.

pub mod ime;
pub mod key;
pub mod mouse;
pub mod select;

pub use ime::{Ime, Preedit};
pub use key::{encode, encode_press};
pub use mouse::{encode_mouse, MouseAction, MouseButton};
pub use select::MouseState;
