//! An MCP server that is a **client of the daemon**, not a feature of the app.
//!
//! Every AI terminal shipping today is a chat sidebar bolted onto a byte
//! stream: the agent scrapes a pty with regular expressions, guesses when a
//! command finished, and drowns in progress-bar noise. zesterm already has the
//! three things that make that shape unnecessary — a headless VT emulator
//! producing typed state (ADR-001), semantic command blocks carrying command,
//! cwd, exit code and timestamps, and a multi-client delta protocol addressing
//! sessions `(HostId, SessionId)` across machines and sealed end to end
//! (ADR-004, ADR-006, ADR-008).
//!
//! So an agent is simply a client. It holds a device key, attaches to a
//! session, receives the deltas the window receives, and writes
//! `ClientMessage::Input`. **No new data plane, no second VT emulator, no
//! privileged in-process surface**, and no protocol version bump: everything
//! here is spelled in messages that already existed. → #60, #274, ADR-015.
//!
//! # What is deliberately not here
//!
//! No agent loop of our own — harnesses exist, improve monthly, and a terminal
//! shipping an inferior one ages badly. Be the substrate.
//!
//! **Nothing that delivers output with no call outstanding.** A "watch this
//! session and react" primitive is what turns prompt injection from *needs the
//! agent to be steered* into *fires on its own*, and its absence is the
//! mitigation rather than an omission.
//!
//! The line is drawn at the call, not at the waiting — `screen` and `blocks`
//! will both block until something happens, which is what makes supervising a
//! build one cheap call instead of a sleep-and-re-read loop. A wait cannot
//! manufacture a turn: the agent asked, the answer is that call's result, and
//! nothing runs afterwards unless the harness grants another turn. What stays
//! unbuilt is anything that speaks when nothing asked. → ADR-015, #319.
//!
//! # Two things the tool results have to keep saying
//!
//! **OSC 133 is forgeable.** Any program can print the markers — `cat` a file
//! containing them and it mints blocks, including a green `exit 0`, and the
//! parser structurally cannot tell. So a block's `command`, `cwd` and
//! `exit_code` are *the shell's word*, and every exit code this server returns
//! carries where it came from. There is exactly one unforgeable exit status in
//! the system and it is `HostMessage::Exited`, which the daemon reads from the
//! child itself.
//!
//! **Terminal output is untrusted input.** A build log, a `curl` of a hostile
//! page, a crafted filename or branch name can all carry text addressed at a
//! model. Two structural defences live here rather than in the harness, because
//! the harness cannot tell which bytes came from a pty: returned text is
//! wrapped in an envelope marked untrusted and fenced with a nonce minted per
//! call, and [`addr::Resolver`] answers only
//! for hosts this process itself listed — so a log line saying "now run this on
//! prod" cannot name a machine.

pub mod addr;
pub mod conn;
pub mod dial;
pub mod fleet;
pub mod keys;
pub mod rpc;
pub mod run;
pub mod session;
pub mod tools;

pub use addr::{AddrError, Resolver};
pub use conn::{Conn, Shared};
pub use dial::{dial, DialError};
pub use fleet::{Fleet, FleetView, LiveFleet, StaticFleet};
pub use keys::{Chord, KeyError};
pub use run::{Anchor, Progress, Refusal};
pub use session::{Replica, StyledSpan};
pub use rpc::{Server, Tools};
pub use tools::{ToolError, ToolSet};

/// How long to wait for a daemon that is not running yet.
///
/// The same budget the app allows, and for the same reason: on the cold path
/// this is a process spawn, and a harness starting a tool server has no window
/// to keep responsive while it happens. Here rather than in `main.rs` because
/// [`dial`] needs it too — a `LocalSocket` route may be the one this server
/// dialled at startup, or another window's on a `--socket` somewhere else.
pub const DAEMON_START: std::time::Duration = std::time::Duration::from_secs(5);

/// What a host shows a person when this client asks to be trusted.
///
/// `Hello.label` is signed into the transcript and is the human's entire
/// decision input at a pairing prompt, so it says plainly what this is. It must
/// never be mistakable for somebody's laptop. Shared by the startup connection
/// and by every fleet dial, because a person approving this agent on a second
/// machine must see the same words they saw on the first.
#[must_use]
pub fn client_label() -> String {
    match std::env::var("MCP_CLIENT_NAME") {
        Ok(h) if !h.trim().is_empty() => format!("zest-mcp agent ({h})"),
        _ => "zest-mcp agent".into(),
    }
}
