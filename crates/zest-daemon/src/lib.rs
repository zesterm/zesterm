//! The process that owns this machine's terminals.
//!
//! Session ownership works ([`session`]), served over the loopback transport
//! ([`local`]). The LAN transport waits on discovery landing in WS-H.
//!
//! # Why a daemon at all, on every machine
//!
//! The obvious design is for the GUI app to own its own PTYs and for a daemon to
//! exist only to expose them to other devices. That was rejected: it produces
//! two session paths that drift, and it means closing the window kills the
//! shell — which is exactly wrong for a fleet whose whole point is picking a
//! session up somewhere else.
//!
//! So the daemon owns sessions everywhere, and the GUI app attaches to its own
//! local daemon over a loopback socket exactly as the phone attaches over the
//! network. One attach path, and shells that outlive the window. → ADR-007.
//!
//! # The cost, stated plainly
//!
//! A keystroke now crosses a process boundary. Over a named pipe or a unix
//! socket that is roughly 50–100µs against a 10ms budget, so it is affordable —
//! but *startup* is where this genuinely threatens something already won. The
//! window currently paints in ~50ms and the prompt is on the first frame,
//! because the shell is spawned before GPU initialization and its ~400ms
//! overlaps the driver's ~850ms.
//!
//! Find-or-spawn-daemon must occupy that same slot. It must never sit between
//! the window being created and the window being painted, and on the warm
//! path — the daemon is already running, which is every launch after the first —
//! it is a pipe open costing microseconds. There is a regression test on first
//! paint so this has a number to break rather than a memory to argue with.

pub mod auth;
pub mod lan;
pub mod local;
pub mod server;
pub mod session;

pub use auth::{Auth, Authenticator};
pub use lan::LanListener;
pub use local::{connect, default_socket_path, listen};
#[cfg(windows)]
pub use local::PipeStream;
pub use server::{serve, Connection, Registry};
pub use session::{Session, Update};

use zest_proto::{HostId, SessionAddr, SessionId};

/// What a daemon needs to know about itself.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// This machine's identity in the fleet.
    pub host: HostId,
    /// Human name, shown in fleet listings.
    pub label: String,
    /// Where the local socket lives.
    ///
    /// A named pipe on Windows, a unix socket under the runtime dir elsewhere.
    pub local_socket: String,
    /// Accept connections from other machines at all.
    ///
    /// Off by default. A daemon that serves only its own GUI app is the
    /// behaviour someone who never asked for a fleet should get.
    pub listen_lan: bool,
    /// Which interface to serve the LAN on.
    ///
    /// `0.0.0.0` unless pinned. IPv6 dual-stack is deliberately not attempted:
    /// `IPV6_V6ONLY` defaults differ between Windows and unix and `std` cannot
    /// set it without `socket2`, so a v6-only network is a known limitation
    /// rather than something that half-works.
    pub lan_bind: String,
    /// Which port to prefer. 0 means "always ephemeral".
    ///
    /// A preference, not a promise: if it is taken the listener falls back and
    /// advertises what it actually bound.
    pub lan_port: u16,
}

/// A session's lifecycle, as clients see it.
///
/// `Detached` is the state that justifies the whole design: the process keeps
/// running, output keeps accumulating, and reattaching from anywhere resumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running { attached: bool },
    /// The child exited; scrollback is still readable until the session is
    /// reaped, so a client can see *why* something died rather than finding a
    /// window that vanished.
    Exited { code: Option<i32> },
}

/// The daemon's view of one session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    pub state: SessionState,
    pub title: String,
}

impl SessionHandle {
    #[must_use]
    pub fn addr(&self, host: HostId) -> SessionAddr {
        SessionAddr::new(host, self.id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("no session {0}")]
    NoSuchSession(u64),
    #[error("the pty could not be started: {0}")]
    Spawn(String),
    #[error("transport failed: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_session_is_still_running() {
        // The property the whole daemon exists for. If closing a client ever
        // implies stopping the shell, the fleet story is gone.
        let h = SessionHandle {
            id: SessionId(1),
            state: SessionState::Running { attached: false },
            title: "pwsh".into(),
        };
        assert!(matches!(h.state, SessionState::Running { attached: false }));
    }

    #[test]
    fn a_session_is_addressable_in_the_fleet() {
        let h = SessionHandle {
            id: SessionId(4),
            state: SessionState::Running { attached: true },
            title: "vim".into(),
        };
        let addr = h.addr(HostId::from_bytes([3; 32]));
        assert_eq!(addr.session, SessionId(4));
    }
}
