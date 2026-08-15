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
//! No streaming or polling tool. A "watch this session and react" primitive is
//! what turns prompt injection from *needs the agent to be steered* into *fires
//! on its own*, and its absence is the mitigation rather than an omission.
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
//! wrapped in an envelope marked untrusted, and [`addr::Resolver`] answers only
//! for hosts this process itself listed — so a log line saying "now run this on
//! prod" cannot name a machine.

pub mod addr;
pub mod session;

pub use addr::{AddrError, Resolver};
pub use session::Replica;
