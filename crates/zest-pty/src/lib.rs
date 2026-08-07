//! Pseudo-terminal creation and I/O.
//!
//! Written directly against ConPTY rather than taking a dependency on
//! `portable-pty`, because ConPTY is the highest-risk component on Windows and
//! its sharp edges have to be controlled rather than abstracted over. See
//! [`windows`] for the specifics.

use std::io::{Read, Write};

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::ConPty as NativePty;

/// Size of the terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// What to run, and how.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Full command line. On Windows this is passed to `CreateProcessW`
    /// verbatim, which parses it itself — we do not attempt to re-quote.
    pub command_line: String,
    /// Working directory; inherits the parent's when `None`.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra environment entries layered over the parent's environment.
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    /// A sensible default shell for the platform.
    #[must_use]
    pub fn default_shell() -> Self {
        // PowerShell 7 if present, else Windows PowerShell, else cmd. Checked at
        // spawn time rather than baked in, because this decides what the user
        // actually gets when they have not configured a shell.
        #[cfg(windows)]
        let command_line = {
            let pwsh = std::path::Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe");
            if pwsh.exists() {
                r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#.to_string()
            } else {
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
            }
        };
        #[cfg(not(windows))]
        let command_line = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());

        Self { command_line, cwd: None, env: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("creating the pseudoconsole failed: {0}")]
    Create(#[source] std::io::Error),
    #[error("spawning `{command}` failed: {source}")]
    Spawn { command: String, #[source] source: std::io::Error },
    #[error("resizing the pseudoconsole failed: {0}")]
    Resize(#[source] std::io::Error),
    #[error("pty i/o failed: {0}")]
    Io(#[from] std::io::Error),
}

/// A source of terminal bytes with a resizable viewport.
///
/// One implementation today. It exists as a trait so the remote milestones can
/// introduce an SSH- or daemon-backed transport without the session layer above
/// having to change shape.
pub trait PtyTransport: Send {
    /// Take the read half. Returns `None` if it has already been taken —
    /// exactly one reader may exist, since two would interleave VT bytes and
    /// corrupt the stream.
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>>;
    /// A write half. Cheap to clone; writes are independently synchronized.
    fn writer(&self) -> Box<dyn Write + Send>;
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;
}
