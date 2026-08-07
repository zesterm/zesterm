//! Pseudo-terminal creation and I/O.
//!
//! Written directly against ConPTY rather than taking a dependency on
//! `portable-pty`, because ConPTY is the highest-risk component on Windows and
//! its sharp edges have to be controlled rather than abstracted over:
//!
//! - `ClosePseudoConsole` **deadlocks** if the output pipe has stopped being
//!   drained ([microsoft/terminal#17688]). Shutdown order must be: signal the
//!   reader, drain to EOF, close the pseudoconsole, then wait on the child.
//! - The child-side pipe handles must be closed in the parent immediately after
//!   `CreateProcessW`, or EOF is never observed.
//! - `ResizePseudoConsole` makes ConPTY re-emit the entire screen, which is a
//!   large byte burst the damage model has to absorb.
//!
//! [microsoft/terminal#17688]: https://github.com/microsoft/terminal/issues/17688

use std::io::{Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to create pseudoconsole: {0}")]
    Create(#[source] std::io::Error),
    #[error("failed to spawn child process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("failed to resize pseudoconsole: {0}")]
    Resize(#[source] std::io::Error),
    #[error("pty i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Size of the terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// A source of terminal bytes with a resizable viewport.
///
/// Defined as a trait now, with exactly one implementation, so that the remote
/// milestones can introduce an SSH- or daemon-backed transport without the
/// session layer above it having to change shape.
pub trait PtyTransport: Send {
    fn reader(&self) -> Result<Box<dyn Read + Send>, PtyError>;
    fn writer(&self) -> Result<Box<dyn Write + Send>, PtyError>;
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;
}
